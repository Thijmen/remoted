//! VideoToolbox low-latency H.264 encoder.
//!
//! One [`H264Encoder`] owns one hardware `VTCompressionSession` (see [`RateControl`] for the two
//! supported configurations). [`H264Encoder::encode`] queues a frame
//! and returns immediately; VideoToolbox later invokes the output callback on its own thread,
//! which copies the AVCC payload, detects keyframes and parameter-set changes and hands an
//! [`EncodedFrame`] to the runtime's [`FrameSink`]. Frames that produce no output are released
//! with [`FrameGate::cancel`]; encoded frames that cannot be delivered with
//! [`FrameGate::post_encode_drop`], after which deltas still inside VideoToolbox are cancelled
//! too until the forced IDR (they reference the lost frame).
//!
//! A session that keeps failing (VideoToolbox reports it invalid or malfunctioning, several
//! frames fail in a row, or a frame gets no output for [`STUCK_AFTER`]) is rebuilt by
//! [`H264Encoder::maintain`], with backoff; one that cannot be brought back is reported by
//! [`H264Encoder::fatal`].

use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    kVTProfileLevel_H264_High_AutoLevel, kVTPropertyNotSupportedErr, kVTVideoEncoderMalfunctionErr,
    kVTVideoEncoderNotAvailableNowErr, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};
use rd_agent_core::h264::codec_string_from_avcc;
use rd_agent_core::{CodecConfig, EncodedFrame, FrameGate, FrameSink};
use tracing::{debug, error, info, warn};

use crate::{cf, vui};

/// "No limit" for the key-frame interval: IDRs only on demand.
const MAX_KEY_FRAME_INTERVAL: i32 = i32::MAX;

/// Rebuild the session after this many frames in a row failed (any non-zero status).
const FAILURES_BEFORE_REBUILD: u32 = 3;
/// Rebuild the session when VideoToolbox has held a frame this long without any output.
pub const STUCK_AFTER: Duration = Duration::from_secs(2);
/// Rebuilds are at least this far apart; the gap doubles after each one up to the maximum,
/// and resets once the session delivers [`HEALTHY_AFTER_FRAMES`].
const REBUILD_BACKOFF_MIN: Duration = Duration::from_secs(1);
const REBUILD_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Rebuild attempts in a row (without a healthy session in between) before the encoder is
/// reported as failed.
const REBUILDS_BEFORE_FATAL: u32 = 3;
/// Delivered frames after which a (re)built session counts as healthy.
const HEALTHY_AFTER_FRAMES: u64 = 10;

/// True for the `n`th occurrence of something worth a log line: 1, 2, 4, 8, ...
fn log_nth(n: u64) -> bool {
    n.is_power_of_two()
}

/// VideoToolbox statuses after which the session is rebuilt right away (WebRTC's
/// RTCVideoEncoderH264 resets on the first two as well).
fn needs_new_session(status: i32) -> bool {
    status == kVTInvalidSessionErr
        || status == kVTVideoEncoderMalfunctionErr
        || status == kVTVideoEncoderNotAvailableNowErr
}

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
    /// Rebuild bookkeeping; carried over to the replacement session.
    recovery: Recovery,
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
            recovery: Recovery::default(),
        })
    }

    /// The bitrate the session was last configured with (bits/s, 0 = encoder default).
    pub fn applied_bitrate(&self) -> u32 {
        self.applied_bitrate
    }

    /// A forced keyframe produced no output, so the next frame will be forced instead. On a
    /// static screen there may be no next frame: the pipeline then re-submits its last one.
    pub fn keyframe_pending(&self) -> bool {
        self.ctx().force_next.load(Ordering::Relaxed)
    }

    /// Why the encoder is considered dead (rebuilding keeps failing), if it is.
    pub fn fatal(&self) -> Option<&str> {
        self.recovery.fatal.as_deref()
    }

    /// Rebuilds the session if it has failed persistently: VideoToolbox reported it invalid
    /// or malfunctioning, [`FAILURES_BEFORE_REBUILD`] frames failed in a row, or a frame has
    /// had no output for [`STUCK_AFTER`]. Rebuilds back off (1 s doubling to 30 s). The new
    /// session starts with an IDR and a new `CodecConfig` (a new epoch).
    ///
    /// Called before every frame, also ones the gate refuses (a stuck session keeps the
    /// window full, so then nothing would be admitted).
    pub fn maintain(&mut self, now: Instant) {
        let Some(reason) = self.ctx().unhealthy(now) else {
            // Only a working session counts: after a failed rebuild the old (broken) one may
            // have delivered plenty before it broke.
            if self.ctx().stats.delivered.load(Ordering::Relaxed) >= HEALTHY_AFTER_FRAMES {
                self.recovery.healthy();
            }
            return;
        };
        if !self.recovery.may_attempt(now) {
            return;
        }
        let attempt = self.recovery.attempts + 1;
        warn!(%reason, attempt, "rebuilding the VideoToolbox session");
        let (gate, sink) = (self.ctx().gate.clone(), self.ctx().sink.clone());
        // The old session is flushed and released when it is replaced; its pending frames are
        // cancelled in the gate.
        match H264Encoder::new(self.cfg, gate, sink, self.bitrate.clone()) {
            Ok(fresh) => {
                let recovery = std::mem::take(&mut self.recovery);
                *self = fresh;
                self.recovery = recovery;
            }
            Err(e) => error!(attempt, "rebuilding the VideoToolbox session failed: {e:#}"),
        }
        self.recovery.attempted(now, &reason);
        if let Some(fatal) = &self.recovery.fatal
            && self.recovery.attempts == REBUILDS_BEFORE_FATAL
        {
            error!("{fatal}");
        }
    }

    /// True if `image` has the size this session encodes.
    pub fn accepts(&self, image: &CVPixelBuffer) -> bool {
        CVPixelBufferGetWidth(image) == self.cfg.width as usize
            && CVPixelBufferGetHeight(image) == self.cfg.height as usize
    }

    /// Queues `image` for encoding. Returns once VideoToolbox has accepted the frame; the
    /// result arrives on the [`FrameSink`]. On failure `seq` has already been cancelled in the
    /// gate. A failing session (e.g. invalidated after a GPU reset) is rebuilt by
    /// [`H264Encoder::maintain`], which runs first.
    pub fn encode(
        &mut self,
        image: &CVPixelBuffer,
        pts_us: u64,
        seq: u32,
        capture_us: u64,
        force_key: bool,
    ) -> anyhow::Result<()> {
        self.maintain(Instant::now());
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
        ctx.failed(status, seq, "VTCompressionSessionEncodeFrame failed");
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
            failed = stats.failed.load(Ordering::Relaxed),
            post_encode_drops = stats.post_encode_drops.load(Ordering::Relaxed),
            chain_drops = stats.chain_drops.load(Ordering::Relaxed),
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
    /// An encoded frame was lost after encoding, so the deltas behind it reference a frame the
    /// viewer never gets. They are cancelled until a keyframe has been delivered.
    chain_broken: AtomicBool,
    /// VideoToolbox reported a status after which the session must be rebuilt (0 = none).
    rebuild_status: AtomicI32,
    /// Frames in a row that failed with a non-zero status; reset by any successful output.
    consecutive_failures: AtomicU32,
    /// Status of the most recent failure.
    last_failure: AtomicI32,
    stats: OutputStats,
}

#[derive(Default)]
struct OutputStats {
    delivered: AtomicU64,
    cancelled: AtomicU64,
    /// Frames that failed with a non-zero status (synchronously or in the callback).
    failed: AtomicU64,
    post_encode_drops: AtomicU64,
    /// Deltas cancelled because they followed a post-encode drop.
    chain_drops: AtomicU64,
    package_errors: AtomicU64,
}

/// Rebuild bookkeeping that outlives a single session.
#[derive(Debug, Default)]
struct Recovery {
    /// Rebuild attempts since the session was last healthy.
    attempts: u32,
    /// No further attempt before this.
    not_before: Option<Instant>,
    backoff: Duration,
    fatal: Option<String>,
}

impl Recovery {
    fn may_attempt(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|t| now >= t)
    }

    /// A rebuild was attempted (whether or not it succeeded).
    fn attempted(&mut self, now: Instant, reason: &str) {
        self.attempts += 1;
        self.backoff = (self.backoff * 2).clamp(REBUILD_BACKOFF_MIN, REBUILD_BACKOFF_MAX);
        self.not_before = Some(now + self.backoff);
        if self.attempts >= REBUILDS_BEFORE_FATAL && self.fatal.is_none() {
            self.fatal = Some(format!(
                "the H.264 encoder keeps failing ({} rebuilds; last problem: {reason})",
                self.attempts
            ));
        }
    }

    fn healthy(&mut self) {
        if self.attempts > 0 || self.fatal.is_some() {
            info!(
                rebuilds = self.attempts,
                "VideoToolbox session healthy again"
            );
        }
        *self = Recovery::default();
    }
}

impl OutputCtx {
    fn new(gate: Arc<dyn FrameGate>, sink: FrameSink) -> Self {
        OutputCtx {
            gate,
            sink,
            pending: parking_lot::Mutex::new(VecDeque::with_capacity(16)),
            format: parking_lot::Mutex::new(FormatState::default()),
            force_next: AtomicBool::new(false),
            chain_broken: AtomicBool::new(false),
            rebuild_status: AtomicI32::new(0),
            consecutive_failures: AtomicU32::new(0),
            last_failure: AtomicI32::new(0),
            stats: OutputStats::default(),
        }
    }

    /// Why the session should be rebuilt, if it should.
    fn unhealthy(&self, now: Instant) -> Option<String> {
        let status = self.rebuild_status.load(Ordering::Relaxed);
        if status != 0 {
            return Some(format!("VideoToolbox error {status}"));
        }
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        if failures >= FAILURES_BEFORE_REBUILD {
            let last = self.last_failure.load(Ordering::Relaxed);
            return Some(format!(
                "{failures} frames failed in a row (last status {last})"
            ));
        }
        let oldest = self.pending.lock().front().map(|f| f.submitted)?;
        let waited = now.saturating_duration_since(oldest);
        (waited >= STUCK_AFTER).then(|| format!("no output for {} ms", waited.as_millis()))
    }

    /// A frame failed with `status` (synchronously or in the callback).
    fn failed(&self, status: i32, seq: u32, what: &str) {
        self.last_failure.store(status, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        if needs_new_session(status) {
            self.rebuild_status.store(status, Ordering::Relaxed);
        }
        let n = self.stats.failed.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
            warn!(status, seq, total = n, "{what}");
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

    /// An encoded frame was lost: the reference chain is broken until the next IDR, which
    /// the gate now forces.
    fn release_encoded(&self, seq: u32) {
        self.chain_broken.store(true, Ordering::Relaxed);
        let n = self.stats.post_encode_drops.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
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
                    self.failed(status, frame.seq, "VideoToolbox failed to encode a frame");
                } else {
                    debug!(seq = frame.seq, "VideoToolbox dropped a frame");
                }
                self.release_unencoded(&frame);
                return;
            }
        };
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let key = is_sync_sample(sample);
        if !self.chain_allows(frame.seq, key) {
            return;
        }
        let config = match self.format.lock().config_for(sample) {
            Ok(config) => config,
            Err(e) => {
                // Nothing was taken from the format state; the next frame tries again.
                self.package_error(frame.seq, &e);
                self.release_encoded(frame.seq);
                return;
            }
        };
        let carries_config = config.is_some();
        let packaged = package(sample, &frame, encode_us, key, config);
        self.deliver(frame.seq, key, carries_config, packaged);
    }

    /// False (and `seq` is released) for a delta encoded before a lost frame was noticed: it
    /// references that frame, so the viewer could not decode it. The IDR that repairs the
    /// chain is already forced, so this cancels rather than forcing another one.
    fn chain_allows(&self, seq: u32, key: bool) -> bool {
        if key || !self.chain_broken.load(Ordering::Relaxed) {
            return true;
        }
        self.stats.chain_drops.fetch_add(1, Ordering::Relaxed);
        debug!(seq, "dropping a delta that follows a lost frame");
        self.gate.cancel(seq);
        false
    }

    /// Hands a packaged frame to the sink. A frame that does not get there breaks the
    /// reference chain (the gate forces an IDR), and the CodecConfig it carried moves to the
    /// next delivered frame, so a config is never lost.
    fn deliver(
        &self,
        seq: u32,
        key: bool,
        carries_config: bool,
        packaged: anyhow::Result<EncodedFrame>,
    ) {
        let delivered = match packaged {
            Ok(encoded) => self.sink.try_send(encoded).is_ok(),
            Err(e) => {
                self.package_error(seq, &e);
                false
            }
        };
        if delivered {
            self.stats.delivered.fetch_add(1, Ordering::Relaxed);
            if key {
                self.chain_broken.store(false, Ordering::Relaxed);
            }
            return;
        }
        if carries_config {
            self.format.lock().resend = true;
        }
        self.release_encoded(seq);
    }

    fn package_error(&self, seq: u32, e: &anyhow::Error) {
        let n = self.stats.package_errors.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
            error!(seq, total = n, "cannot package encoded frame: {e:#}");
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
    key: bool,
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
        key,
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
        Ok(self.take_resend())
    }

    /// The config, if it is due to be (re)sent.
    fn take_resend(&mut self) -> Option<CodecConfig> {
        if std::mem::take(&mut self.resend) {
            self.config.clone()
        } else {
            None
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use parking_lot::Mutex;
    use rd_agent_core::Admitted;
    use tokio::sync::mpsc;

    use super::*;

    #[derive(Default)]
    struct TestGate {
        cancelled: Mutex<Vec<u32>>,
        post_dropped: Mutex<Vec<u32>>,
        admitted: AtomicU64,
    }

    impl FrameGate for TestGate {
        fn admit(&self, _capture_us: u64) -> Option<Admitted> {
            let n = self.admitted.fetch_add(1, Ordering::Relaxed) as u32 + 1;
            Some(Admitted {
                seq: n,
                force_keyframe: false,
            })
        }
        fn cancel(&self, seq: u32) {
            self.cancelled.lock().push(seq);
        }
        fn post_encode_drop(&self, seq: u32) {
            self.post_dropped.lock().push(seq);
        }
    }

    fn config() -> CodecConfig {
        CodecConfig {
            codec: "avc1.640033".into(),
            width: 320,
            height: 192,
            avcc: Bytes::from_static(&[1, 0x64, 0, 0x33]),
        }
    }

    fn frame(seq: u32, key: bool, config: Option<CodecConfig>) -> EncodedFrame {
        EncodedFrame {
            seq,
            capture_us: 0,
            encode_us: 0,
            key,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            config,
        }
    }

    fn ctx(capacity: usize) -> (Arc<TestGate>, OutputCtx, mpsc::Receiver<EncodedFrame>) {
        let gate = Arc::new(TestGate::default());
        let (tx, rx) = mpsc::channel(capacity);
        let ctx = OutputCtx::new(gate.clone(), tx);
        (gate, ctx, rx)
    }

    /// R4/L1: the frame carrying a CodecConfig fails to package; the config goes out with the
    /// next delivered frame (the forced IDR), and nothing in between is delivered.
    #[test]
    fn config_survives_a_failed_frame() {
        let (gate, ctx, mut rx) = ctx(8);
        {
            let mut format = ctx.format.lock();
            format.config = Some(config());
            format.resend = true;
        }
        let first = ctx.format.lock().take_resend();
        assert_eq!(first, Some(config()));
        ctx.deliver(
            1,
            true,
            true,
            Err(anyhow::anyhow!("CMBlockBufferCopyDataBytes failed")),
        );
        assert_eq!(*gate.post_dropped.lock(), [1], "an IDR is forced");
        assert!(ctx.chain_broken.load(Ordering::Relaxed));
        assert!(rx.try_recv().is_err());

        // The next IDR takes the config along.
        let again = ctx.format.lock().take_resend();
        assert_eq!(again, Some(config()));
        ctx.deliver(2, true, true, Ok(frame(2, true, again)));
        let got = rx.try_recv().unwrap();
        assert!(got.key && got.config == Some(config()));
        assert!(!ctx.chain_broken.load(Ordering::Relaxed));
        assert_eq!(ctx.format.lock().take_resend(), None, "sent once");
    }

    /// R3: deltas already inside VideoToolbox when a frame is lost are cancelled (not
    /// delivered, not re-forcing an IDR) until a keyframe has been delivered.
    #[test]
    fn deltas_after_a_lost_frame_are_cancelled_until_the_idr() {
        let (gate, ctx, mut rx) = ctx(1);
        assert!(ctx.chain_allows(1, false));
        ctx.deliver(1, false, false, Ok(frame(1, false, None)));
        // Sink full: frame 2 is lost after encoding.
        assert!(ctx.chain_allows(2, false));
        ctx.deliver(2, false, false, Ok(frame(2, false, None)));
        assert_eq!(*gate.post_dropped.lock(), [2]);
        assert!(rx.try_recv().is_ok_and(|f| f.seq == 1));
        // Frames 3 and 4 were encoded before the drop: they reference frame 2.
        assert!(!ctx.chain_allows(3, false));
        assert!(!ctx.chain_allows(4, false));
        assert_eq!(*gate.cancelled.lock(), [3, 4]);
        assert_eq!(*gate.post_dropped.lock(), [2], "no second forced IDR");
        // The forced IDR goes through and repairs the chain.
        assert!(ctx.chain_allows(5, true));
        ctx.deliver(5, true, false, Ok(frame(5, true, None)));
        assert!(rx.try_recv().is_ok_and(|f| f.seq == 5 && f.key));
        assert!(ctx.chain_allows(6, false));
        assert_eq!(ctx.stats.chain_drops.load(Ordering::Relaxed), 2);
    }

    /// A keyframe that cannot be delivered keeps the chain broken.
    #[test]
    fn a_lost_keyframe_keeps_the_chain_broken() {
        let (gate, ctx, _rx) = ctx(1);
        ctx.deliver(1, true, false, Ok(frame(1, true, None)));
        ctx.deliver(2, true, false, Ok(frame(2, true, None)));
        assert_eq!(*gate.post_dropped.lock(), [2]);
        assert!(!ctx.chain_allows(3, false));
    }

    /// The same when the sink is full.
    #[test]
    fn config_survives_a_full_sink() {
        let (gate, ctx, mut rx) = ctx(1);
        ctx.deliver(1, false, false, Ok(frame(1, false, None)));
        ctx.format.lock().config = Some(config());
        ctx.deliver(2, true, true, Ok(frame(2, true, Some(config()))));
        assert_eq!(*gate.post_dropped.lock(), [2]);
        assert!(ctx.format.lock().resend);
        assert!(rx.try_recv().is_ok_and(|f| f.seq == 1));
    }

    #[test]
    fn rebuild_reasons() {
        let (_gate, ctx, _rx) = ctx(8);
        let now = Instant::now();
        assert_eq!(ctx.unhealthy(now), None);

        // Ordinary failures: only after several in a row.
        for _ in 1..FAILURES_BEFORE_REBUILD {
            ctx.failed(-12902, 1, "test");
        }
        assert_eq!(ctx.unhealthy(now), None);
        ctx.failed(-12902, 1, "test");
        assert!(ctx.unhealthy(now).unwrap().contains("in a row"));
        ctx.consecutive_failures.store(0, Ordering::Relaxed);

        // Statuses that mean the session is gone: at once.
        for status in [
            kVTInvalidSessionErr,
            kVTVideoEncoderMalfunctionErr,
            kVTVideoEncoderNotAvailableNowErr,
        ] {
            assert!(needs_new_session(status));
            let (_gate, ctx, _rx) = self::ctx(8);
            ctx.failed(status, 1, "test");
            assert!(ctx.unhealthy(now).unwrap().contains(&status.to_string()));
        }
        assert!(!needs_new_session(-12902));

        // A frame without output for too long.
        ctx.pending.lock().push_back(PendingFrame {
            token: 1,
            seq: 1,
            capture_us: 0,
            force_key: false,
            submitted: now,
        });
        assert_eq!(ctx.unhealthy(now + STUCK_AFTER / 2), None);
        assert!(
            ctx.unhealthy(now + STUCK_AFTER)
                .unwrap()
                .contains("no output")
        );
    }

    #[test]
    fn recovery_backs_off_and_gives_up() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        assert!(r.may_attempt(t0));
        r.attempted(t0, "x");
        assert!(!r.may_attempt(t0 + REBUILD_BACKOFF_MIN / 2));
        assert!(r.may_attempt(t0 + REBUILD_BACKOFF_MIN));
        let t1 = t0 + REBUILD_BACKOFF_MIN;
        r.attempted(t1, "x");
        assert!(!r.may_attempt(t1 + REBUILD_BACKOFF_MIN));
        assert!(r.may_attempt(t1 + REBUILD_BACKOFF_MIN * 2));
        assert_eq!(r.fatal, None);
        r.attempted(t1, "stuck");
        assert!(r.fatal.as_deref().unwrap().contains("stuck"));
        for _ in 0..20 {
            r.attempted(t1, "x");
        }
        assert_eq!(r.backoff, REBUILD_BACKOFF_MAX);
        r.healthy();
        assert!(r.may_attempt(t1) && r.fatal.is_none() && r.attempts == 0);
    }

    fn test_image(width: u32, height: u32) -> CFRetained<CVPixelBuffer> {
        crate::pattern::TestPattern::new(width, height)
            .unwrap()
            .render(0, 0)
            .unwrap()
    }

    fn recv(rx: &mut mpsc::Receiver<EncodedFrame>) -> EncodedFrame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(f) = rx.try_recv() {
                return f;
            }
            assert!(Instant::now() < deadline, "no frame");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// M2: a session VideoToolbox reports as malfunctioning is replaced (new epoch: config +
    /// IDR), rebuilds back off, and repeated rebuilds without recovery are reported.
    #[test]
    fn malfunctioning_session_is_rebuilt() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = EncoderConfig {
            width: 320,
            height: 192,
            fps: 60,
            rate_control: RateControl::Standard,
        };
        let mut enc =
            H264Encoder::new(cfg, gate.clone(), tx, Arc::new(AtomicU32::new(1_000_000))).unwrap();
        let image = test_image(320, 192);
        let mut seq = 0;
        let mut encode = |enc: &mut H264Encoder| {
            seq += 1;
            enc.encode(&image, u64::from(seq) * 16_000, seq, 0, false)
                .unwrap();
        };
        encode(&mut enc);
        assert!(recv(&mut rx).config.is_some());
        encode(&mut enc);
        assert!(recv(&mut rx).config.is_none());

        let t0 = Instant::now();
        let before = enc.ctx;
        enc.ctx().failed(kVTVideoEncoderMalfunctionErr, 0, "test");
        enc.maintain(t0);
        assert_ne!(enc.ctx, before, "a new session");
        assert_eq!(enc.recovery.attempts, 1);
        encode(&mut enc);
        let f = recv(&mut rx);
        assert!(f.key && f.config.is_some(), "a new epoch");

        // Within the backoff: no rebuild.
        let before = enc.ctx;
        enc.ctx().failed(kVTInvalidSessionErr, 0, "test");
        enc.maintain(t0);
        assert_eq!(enc.ctx, before);
        enc.maintain(t0 + REBUILD_BACKOFF_MIN);
        assert_ne!(enc.ctx, before);
        assert_eq!(enc.fatal(), None);

        enc.ctx().failed(kVTInvalidSessionErr, 0, "test");
        enc.maintain(t0 + REBUILD_BACKOFF_MIN * 4);
        assert!(enc.fatal().is_some(), "{:?}", enc.recovery);

        // Healthy again after enough delivered frames.
        for _ in 0..HEALTHY_AFTER_FRAMES {
            encode(&mut enc);
            recv(&mut rx);
        }
        enc.maintain(Instant::now());
        assert_eq!(enc.fatal(), None);
        assert_eq!(enc.recovery.attempts, 0);
        assert!(gate.post_dropped.lock().is_empty());
    }
}
