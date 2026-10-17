//! VideoToolbox low-latency H.264 encoder.
//!
//! One [`H264Encoder`] owns one hardware `VTCompressionSession` (see [`RateControl`] for the two
//! supported configurations). [`H264Encoder::encode`] queues a frame
//! and returns immediately; VideoToolbox later invokes the output callback on its own thread,
//! which copies the AVCC payload, detects keyframes and parameter-set changes and hands an
//! [`EncodedFrame`] to the runtime's [`FrameSink`]. Frames that produce no output are released
//! with [`FrameGate::cancel`]; encoded frames that cannot be delivered with
//! [`FrameGate::post_encode_drop`].

use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context as _, bail};
use bytes::Bytes;
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetDimensions,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTInvalidSessionErr, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTPropertyNotSupportedErr,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};
use rd_agent_core::h264::codec_string_from_avcc;
use rd_agent_core::{CodecConfig, EncodedFrame, FrameGate, FrameSink};
use tracing::{debug, error, info, warn};

use crate::{cf, vui};

/// "No limit" for the key-frame interval: IDRs only on demand.
const MAX_KEY_FRAME_INTERVAL: i32 = i32::MAX;

/// Environment override for the default [`RateControl`]: `low-latency` or `standard`.
pub const RATE_CONTROL_ENV: &str = "RD_VT_RATE_CONTROL";

/// How the session is configured. Both modes use the hardware encoder, ConstrainedHigh (or
/// High), no frame reordering, `AverageBitRate`, IDR only on demand and BT.709 tags.
///
/// Measured on an M1 Pro (macOS 26.3, 1728x1118@60, synthetic content, 2026-10-17):
/// - `Standard`: p50 ~9.3 ms, p90 ~14 ms, no drops. IDRs ~250 KiB (vs ~177 KiB) and looser
///   per-second bitrate adherence. Its SPS lacks the VUI bitstream restriction, which the
///   encoder then adds to the avcC itself (see `vui.rs`).
/// - `LowLatency`: p50 ~19 ms, p90 ~28-36 ms. `RealTime = true` makes VideoToolbox pace itself
///   to `ExpectedFrameRate` (about one frame interval); `Standard` with `RealTime = true` shows
///   the same ~17 ms. Overstating `ExpectedFrameRate` (90-240) lowers p50 to 10-12 ms but p90
///   stays above 25 ms and the encoder starts dropping frames.
///
/// So `Standard` is the default; `LowLatency` stays selectable for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateControl {
    /// `EnableLowLatencyRateControl` + `RealTime = true` (the original plan's configuration).
    LowLatency,
    /// Default rate control with `RealTime = false` and `PrioritizeEncodingSpeedOverQuality`:
    /// the encoder runs flat out instead of pacing itself to the frame rate.
    #[default]
    Standard,
}

impl RateControl {
    /// The default, unless overridden with `RD_VT_RATE_CONTROL=standard|low-latency`.
    pub fn from_env() -> Self {
        match std::env::var(RATE_CONTROL_ENV).as_deref() {
            Ok("standard") | Err(_) => RateControl::Standard,
            Ok("low-latency") => RateControl::LowLatency,
            Ok(other) => {
                warn!("{RATE_CONTROL_ENV}={other:?} is not low-latency|standard; using standard");
                RateControl::Standard
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RateControl::LowLatency => "low-latency",
            RateControl::Standard => "standard",
        }
    }
}

impl std::str::FromStr for RateControl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "low-latency" => Ok(RateControl::LowLatency),
            "standard" => Ok(RateControl::Standard),
            _ => bail!("unknown rate control {s:?} (expected low-latency or standard)"),
        }
    }
}

/// Encoder parameters fixed for the lifetime of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate_control: RateControl,
}

/// Bitrate target shared between a pipeline and its (possibly replaced) encoders. Written by
/// `set_bitrate` from any thread; applied by the encoder before its next frame.
pub type BitrateTarget = Arc<AtomicU32>;

/// A low-latency hardware H.264 `VTCompressionSession`.
pub struct H264Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Boxed callback state; VideoToolbox holds the raw pointer as `outputCallbackRefCon`.
    /// Freed in `Drop` after the session has been invalidated.
    ctx: NonNull<OutputCtx>,
    cfg: EncoderConfig,
    bitrate: BitrateTarget,
    applied_bitrate: u32,
    force_key_props: CFRetained<CFDictionary<CFString, CFType>>,
    next_token: u64,
    last_pts_us: Option<u64>,
}

// SAFETY: a VTCompressionSession and an immutable CFDictionary may be used from any thread;
// all access to them goes through `&mut self`, so it is never concurrent. `OutputCtx` is only
// touched through its Mutex/atomic fields and its own thread-safe handles.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    /// Creates and configures a session. Fails if no hardware encoder is available.
    pub fn new(
        cfg: EncoderConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
    ) -> anyhow::Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            bail!("invalid encoder config {cfg:?}");
        }
        let ctx = NonNull::from(Box::leak(Box::new(OutputCtx::new(gate, sink))));
        let created = create_session(&cfg, ctx);
        let session = match created {
            Ok(session) => session,
            Err(e) => {
                // SAFETY: the session was not created (or already released), so nothing else
                // references `ctx`.
                drop(unsafe { Box::from_raw(ctx.as_ptr()) });
                return Err(e);
            }
        };
        let applied_bitrate = bitrate.load(Ordering::Relaxed);
        let profile = configure_session(&session, &cfg, applied_bitrate);
        // The low-latency encoder does not expose this property; creation with
        // RequireHardwareAcceleratedVideoEncoder already guarantees hardware.
        let hardware = copy_bool_property(
            &session,
            // SAFETY: extern static provided by VideoToolbox.
            unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder },
        )
        .unwrap_or(true);
        // SAFETY: valid session.
        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            warn!(status, "VTCompressionSessionPrepareToEncodeFrames failed");
        }
        // SAFETY: extern static provided by VideoToolbox.
        let force_key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let force_key_props = cf::dict([(force_key, CFBoolean::new(true))]);
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_bps = applied_bitrate,
            rate_control = cfg.rate_control.as_str(),
            profile,
            hardware,
            "VideoToolbox H.264 session created"
        );
        Ok(H264Encoder {
            session,
            ctx,
            cfg,
            bitrate,
            applied_bitrate,
            force_key_props,
            next_token: 1,
            last_pts_us: None,
        })
    }

    /// True if `image` has the size this session encodes.
    pub fn accepts(&self, image: &CVPixelBuffer) -> bool {
        CVPixelBufferGetWidth(image) == self.cfg.width as usize
            && CVPixelBufferGetHeight(image) == self.cfg.height as usize
    }

    /// Queues `image` for encoding. Returns once VideoToolbox has accepted the frame; the
    /// result arrives on the [`FrameSink`]. On failure `seq` has already been cancelled in the
    /// gate. An invalidated session (e.g. after a GPU reset) is rebuilt for the next frame.
    pub fn encode(
        &mut self,
        image: &CVPixelBuffer,
        pts_us: u64,
        seq: u32,
        capture_us: u64,
        force_key: bool,
    ) -> anyhow::Result<()> {
        self.apply_bitrate();
        // VideoToolbox requires strictly increasing presentation timestamps.
        let pts_us = match self.last_pts_us {
            Some(last) if pts_us <= last => last + 1,
            _ => pts_us,
        };
        self.last_pts_us = Some(pts_us);
        let token = self.next_token;
        self.next_token += 1;

        let ctx = self.ctx();
        let force_key = force_key | ctx.force_next.swap(false, Ordering::Relaxed);
        ctx.pending.lock().push_back(PendingFrame {
            token,
            seq,
            capture_us,
            force_key,
            submitted: Instant::now(),
        });

        let pts = CMTime {
            value: pts_us as i64,
            timescale: 1_000_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let props = force_key.then(|| self.force_key_props.as_opaque());
        // SAFETY: valid session and pixel buffer; `token` is an opaque integer carried through
        // `sourceFrameRefCon` (never dereferenced). The callback runs on another thread and
        // only touches `OutputCtx`, which outlives the session.
        let status = unsafe {
            self.session.encode_frame(
                image,
                pts,
                kCMTimeInvalid,
                props,
                token as usize as *mut c_void,
                ptr::null_mut(),
            )
        };
        if status == 0 {
            return Ok(());
        }
        // The callback may or may not have run for a synchronous failure; whoever removes the
        // pending entry releases the frame.
        if let Some(frame) = ctx.take_pending(token) {
            ctx.release_unencoded(&frame);
        }
        if status == kVTInvalidSessionErr {
            warn!("VideoToolbox session invalidated; recreating it");
            let (gate, sink) = (ctx.gate.clone(), ctx.sink.clone());
            *self = H264Encoder::new(self.cfg, gate, sink, self.bitrate.clone())?;
        }
        bail!("VTCompressionSessionEncodeFrame failed: {status}")
    }

    fn ctx(&self) -> &OutputCtx {
        // SAFETY: `ctx` is a live Box until `Drop` frees it.
        unsafe { self.ctx.as_ref() }
    }

    fn apply_bitrate(&mut self) {
        let want = self.bitrate.load(Ordering::Relaxed);
        if want == 0 || want == self.applied_bitrate {
            return;
        }
        // SAFETY: extern static provided by VideoToolbox.
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        set_property(&self.session, key, &CFNumber::new_i64(i64::from(want)));
        // Even on failure: retrying every frame would only spam the log.
        self.applied_bitrate = want;
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: valid session. CompleteFrames emits every pending frame before returning;
        // after Invalidate VideoToolbox makes no further callbacks, so freeing `ctx` is sound.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
        let ctx = unsafe { Box::from_raw(self.ctx.as_ptr()) };
        for frame in ctx.pending.lock().drain(..) {
            ctx.gate.cancel(frame.seq);
        }
        let stats = &ctx.stats;
        debug!(
            delivered = stats.delivered.load(Ordering::Relaxed),
            cancelled = stats.cancelled.load(Ordering::Relaxed),
            post_encode_drops = stats.post_encode_drops.load(Ordering::Relaxed),
            "VideoToolbox session closed"
        );
    }
}

fn create_session(
    cfg: &EncoderConfig,
    ctx: NonNull<OutputCtx>,
) -> anyhow::Result<CFRetained<VTCompressionSession>> {
    // SAFETY (all statics below): extern constants provided by VideoToolbox / CoreVideo.
    let spec = cf::dict([
        (
            unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
            CFBoolean::new(cfg.rate_control == RateControl::LowLatency),
        ),
        (
            unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder },
            CFBoolean::new(true),
        ),
    ]);
    let io_surface = CFDictionary::<CFString, CFType>::empty();
    let source_attrs = cf::dict([
        (
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            &CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32),
        ),
        (
            unsafe { kCVPixelBufferWidthKey },
            &CFNumber::new_i32(cfg.width as i32),
        ),
        (
            unsafe { kCVPixelBufferHeightKey },
            &CFNumber::new_i32(cfg.height as i32),
        ),
        (unsafe { kCVPixelBufferIOSurfacePropertiesKey }, &io_surface),
    ]);
    let mut out: *mut VTCompressionSession = ptr::null_mut();
    // SAFETY: dictionaries are valid CF objects with CFString keys; `output_callback` matches
    // the prototype; `ctx` stays valid until the session is invalidated.
    let status = unsafe {
        VTCompressionSession::create(
            None,
            cfg.width as i32,
            cfg.height as i32,
            kCMVideoCodecType_H264,
            Some(spec.as_opaque()),
            Some(source_attrs.as_opaque()),
            None,
            Some(output_callback),
            ctx.as_ptr().cast(),
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        bail!(
            "VTCompressionSessionCreate({}x{}, {}, hardware) failed: {status}",
            cfg.width,
            cfg.height,
            cfg.rate_control.as_str()
        );
    }
    let out = NonNull::new(out).context("VTCompressionSessionCreate returned no session")?;
    // SAFETY: Create returns a +1 reference.
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// Applies the session properties for `cfg`; returns the accepted profile name. Failures are
/// logged and otherwise tolerated (the session still encodes).
fn configure_session(
    session: &VTCompressionSession,
    cfg: &EncoderConfig,
    bps: u32,
) -> &'static str {
    // SAFETY (all statics below): extern constants provided by VideoToolbox / CoreVideo.
    let t: &CFType = CFBoolean::new(true);
    let f: &CFType = CFBoolean::new(false);
    // RealTime lets the encoder pace itself to ExpectedFrameRate; the standard rate control
    // only reaches its low latency without it. (ExpectedFrameRate must never be 0: the
    // low-latency rate control then drops every frame.)
    let real_time = match cfg.rate_control {
        RateControl::LowLatency => t,
        RateControl::Standard => f,
    };
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_RealTime },
        real_time,
    );

    let profile_key = unsafe { kVTCompressionPropertyKey_ProfileLevel };
    let profile = if set_property(session, profile_key, unsafe {
        kVTProfileLevel_H264_ConstrainedHigh_AutoLevel
    }) {
        "ConstrainedHigh"
    } else if set_property(session, profile_key, unsafe {
        kVTProfileLevel_H264_High_AutoLevel
    }) {
        "High"
    } else {
        "default"
    };

    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
        f,
    );
    if bps > 0 {
        set_property(
            session,
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            &CFNumber::new_i64(i64::from(bps)),
        );
    }
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
        &CFNumber::new_i32(cfg.fps as i32),
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
        &CFNumber::new_i32(MAX_KEY_FRAME_INTERVAL),
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
        &CFNumber::new_f64(0.0),
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
        t,
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_ColorPrimaries },
        unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 },
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_TransferFunction },
        unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 },
    );
    set_property(
        session,
        unsafe { kVTCompressionPropertyKey_YCbCrMatrix },
        unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 },
    );
    profile
}

/// `VTSessionSetProperty`, logging failures. Returns true on success.
fn set_property(session: &VTCompressionSession, key: &CFString, value: &CFType) -> bool {
    // SAFETY: valid session; `value` has the type documented for `key` at every call site.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    match status {
        0 => {}
        // e.g. PrioritizeEncodingSpeedOverQuality with the low-latency rate control.
        s if s == kVTPropertyNotSupportedErr => {
            info!(property = %key, "not supported by this encoder")
        }
        _ => warn!(property = %key, status, "VTSessionSetProperty failed"),
    }
    status == 0
}

fn copy_bool_property(session: &VTCompressionSession, key: &CFString) -> Option<bool> {
    let mut out: *const CFType = ptr::null();
    // SAFETY: `out` receives a +1 CF object (or stays NULL).
    let status =
        unsafe { VTSessionCopyProperty(session, key, None, (&raw mut out).cast::<c_void>()) };
    let value = NonNull::new(out.cast_mut()).filter(|_| status == 0)?;
    // SAFETY: Copy returns a +1 reference.
    let value = unsafe { CFRetained::from_raw(value) };
    value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}

// ---------------------------------------------------------------------------
// output callback

struct PendingFrame {
    token: u64,
    seq: u32,
    capture_us: u64,
    force_key: bool,
    submitted: Instant,
}

/// State shared between [`H264Encoder::encode`] and the VideoToolbox output callback.
struct OutputCtx {
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    /// Frames queued in VideoToolbox, in submission (= decode) order.
    pending: parking_lot::Mutex<VecDeque<PendingFrame>>,
    format: parking_lot::Mutex<FormatState>,
    /// A forced keyframe produced no output: force the next frame instead.
    force_next: AtomicBool,
    stats: OutputStats,
}

#[derive(Default)]
struct OutputStats {
    delivered: AtomicU64,
    cancelled: AtomicU64,
    post_encode_drops: AtomicU64,
}

impl OutputCtx {
    fn new(gate: Arc<dyn FrameGate>, sink: FrameSink) -> Self {
        OutputCtx {
            gate,
            sink,
            pending: parking_lot::Mutex::new(VecDeque::with_capacity(16)),
            format: parking_lot::Mutex::new(FormatState::default()),
            force_next: AtomicBool::new(false),
            stats: OutputStats::default(),
        }
    }

    fn take_pending(&self, token: u64) -> Option<PendingFrame> {
        let mut pending = self.pending.lock();
        let i = pending.iter().position(|p| p.token == token)?;
        pending.remove(i)
    }

    /// An admitted frame produced no output.
    fn release_unencoded(&self, frame: &PendingFrame) {
        if frame.force_key {
            self.force_next.store(true, Ordering::Relaxed);
        }
        self.stats.cancelled.fetch_add(1, Ordering::Relaxed);
        self.gate.cancel(frame.seq);
    }

    /// An encoded frame was lost: the reference chain is broken.
    fn release_encoded(&self, seq: u32) {
        let n = self.stats.post_encode_drops.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            warn!(seq, total = n, "encoded frame dropped; forcing an IDR");
        }
        self.gate.post_encode_drop(seq);
    }

    fn on_output(
        &self,
        token: u64,
        status: i32,
        flags: VTEncodeInfoFlags,
        sample: Option<&CMSampleBuffer>,
    ) {
        let Some(frame) = self.take_pending(token) else {
            return; // already released by a synchronous encode failure
        };
        let encode_us = u32::try_from(frame.submitted.elapsed().as_micros()).unwrap_or(u32::MAX);
        let sample = match sample {
            Some(s) if status == 0 && !flags.contains(VTEncodeInfoFlags::FrameDropped) => s,
            _ => {
                if status != 0 {
                    warn!(
                        status,
                        seq = frame.seq,
                        "VideoToolbox failed to encode a frame"
                    );
                } else {
                    debug!(seq = frame.seq, "VideoToolbox dropped a frame");
                }
                self.release_unencoded(&frame);
                return;
            }
        };
        let packaged = self
            .format
            .lock()
            .config_for(sample)
            .and_then(|config| package(sample, &frame, encode_us, config));
        let encoded = match packaged {
            Ok(f) => f,
            Err(e) => {
                error!(seq = frame.seq, "cannot package encoded frame: {e:#}");
                self.release_encoded(frame.seq);
                return;
            }
        };
        let carried_config = encoded.config.is_some();
        match self.sink.try_send(encoded) {
            Ok(()) => {
                self.stats.delivered.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                if carried_config {
                    self.format.lock().resend = true;
                }
                self.release_encoded(frame.seq);
            }
        }
    }
}

/// VideoToolbox output callback (`VTCompressionOutputCallback`).
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the encoder's `OutputCtx`, alive until the session is invalidated.
    let ctx = unsafe { &*refcon.cast::<OutputCtx>() };
    // SAFETY: VideoToolbox passes a valid sample buffer (or NULL) for the callback's duration.
    let sample = unsafe { sample.as_ref() };
    let token = frame_refcon as usize as u64;
    // Never unwind into VideoToolbox.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        ctx.on_output(token, status, flags, sample)
    }));
    if result.is_err() {
        error!("panic in VideoToolbox output callback");
    }
}

/// Copies one encoded access unit out of a sample buffer.
fn package(
    sample: &CMSampleBuffer,
    frame: &PendingFrame,
    encode_us: u32,
    config: Option<CodecConfig>,
) -> anyhow::Result<EncodedFrame> {
    // SAFETY: valid sample buffer; Get functions return +0 references that the bindings retain.
    let block = unsafe { sample.data_buffer() }.context("sample has no data buffer")?;
    let len = unsafe { block.data_length() };
    if len == 0 {
        bail!("empty encoded sample");
    }
    let mut data = Vec::<u8>::with_capacity(len);
    // SAFETY: `data` has capacity for `len` bytes, all of which CopyDataBytes writes on success.
    let status =
        unsafe { block.copy_data_bytes(0, len, NonNull::new_unchecked(data.as_mut_ptr().cast())) };
    if status != 0 {
        bail!("CMBlockBufferCopyDataBytes failed: {status}");
    }
    unsafe { data.set_len(len) };
    Ok(EncodedFrame {
        seq: frame.seq,
        capture_us: frame.capture_us,
        encode_us,
        key: is_sync_sample(sample),
        data: Bytes::from(data),
        config,
    })
}

/// A sample is a keyframe unless its attachments carry `NotSync = true`.
fn is_sync_sample(sample: &CMSampleBuffer) -> bool {
    // SAFETY: valid sample buffer; the array is retained by the binding.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // SAFETY: the attachments array holds CF objects (one dictionary per sample).
    let attachments = unsafe {
        CFRetained::cast_unchecked::<objc2_core_foundation::CFArray<CFType>>(attachments)
    };
    let Some(first) = attachments.get(0) else {
        return true;
    };
    let Some(first) = first.downcast_ref::<CFDictionary>() else {
        return true;
    };
    // SAFETY: extern static provided by CoreMedia.
    !cf::dict_bool(first, unsafe { kCMSampleAttachmentKey_NotSync }).unwrap_or(false)
}

/// Retained format description, compared by identity to detect parameter-set changes cheaply.
struct FormatDesc(CFRetained<CMFormatDescription>);

// SAFETY: CMFormatDescription is immutable and thread-safe.
unsafe impl Send for FormatDesc {}

#[derive(Default)]
struct FormatState {
    desc: Option<FormatDesc>,
    config: Option<CodecConfig>,
    /// Attach `config` to the next delivered frame.
    resend: bool,
}

impl FormatState {
    /// The `CodecConfig` to attach to this frame: `Some` for the first frame, when the
    /// parameter sets change, or after a frame carrying the config was lost.
    fn config_for(&mut self, sample: &CMSampleBuffer) -> anyhow::Result<Option<CodecConfig>> {
        // SAFETY: valid sample buffer.
        let desc = unsafe { sample.format_description() }.context("no format description")?;
        let same_object = self
            .desc
            .as_ref()
            .is_some_and(|d| ptr::eq::<CMFormatDescription>(&*d.0, &*desc));
        if !same_object {
            let config = codec_config(&desc)?;
            if self.config.as_ref() != Some(&config) {
                info!(codec = %config.codec, width = config.width, height = config.height,
                    avcc_len = config.avcc.len(), "encoder parameter sets");
                self.config = Some(config);
                self.resend = true;
            }
            self.desc = Some(FormatDesc(desc));
        }
        if std::mem::take(&mut self.resend) {
            Ok(self.config.clone())
        } else {
            Ok(None)
        }
    }
}

fn codec_config(desc: &CMFormatDescription) -> anyhow::Result<CodecConfig> {
    // SAFETY: valid format description; extern static provided by CoreMedia.
    let atoms =
        unsafe { desc.extension(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) }
            .context("format description has no SampleDescriptionExtensionAtoms")?;
    let atoms = atoms
        .downcast_ref::<CFDictionary>()
        .context("SampleDescriptionExtensionAtoms is not a dictionary")?;
    let avcc = cf::dict_get(atoms, &CFString::from_static_str("avcC"))
        .context("no avcC atom in the format description")?;
    let avcc = avcc
        .downcast_ref::<CFData>()
        .context("avcC atom is not CFData")?
        .to_vec();
    let avcc = match vui::without_reordering(&avcc) {
        Ok(Some(rewritten)) => {
            info!("SPS VUI lacked max_num_reorder_frames = 0; added it to the avcC");
            rewritten
        }
        Ok(None) => avcc,
        Err(e) => {
            warn!("cannot inspect the SPS VUI: {e:#}");
            avcc
        }
    };
    let codec = codec_string_from_avcc(&avcc).context("malformed avcC record")?;
    // SAFETY: valid video format description.
    let dims = unsafe { CMVideoFormatDescriptionGetDimensions(desc) };
    Ok(CodecConfig {
        codec,
        width: u32::try_from(dims.width).context("negative width")?,
        height: u32::try_from(dims.height).context("negative height")?,
        avcc: Bytes::from(avcc),
    })
}
