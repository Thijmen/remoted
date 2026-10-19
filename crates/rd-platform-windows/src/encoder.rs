//! Media Foundation H.264 encoder.
//!
//! One [`H264Encoder`] drives one encoder MFT. The preferred one is the GPU vendor's hardware
//! MFT (NVENC, Quick Sync, AMF) on the adapter that holds the frames: an *asynchronous* MFT fed
//! with NV12 D3D11 textures from a small pool ([`H264Encoder::acquire_surface`]). Without one,
//! Microsoft's software encoder (a synchronous MFT) is fed from system memory, reading frames
//! back from the GPU when they live there.
//!
//! Both kinds produce Annex-B access units. The output side turns them into what the runtime
//! wants: AVCC payloads without in-band parameter sets, an avcC record built from the SPS/PPS
//! (with the VUI fix-up of [`rd_agent_core::h264::vui`]) and the WebCodecs codec string, sent as
//! a new [`CodecConfig`] whenever the parameter sets change.
//!
//! Frame accounting follows the VideoToolbox encoder: frames the MFT never outputs are released
//! with [`FrameGate::cancel`]; encoded frames that cannot be delivered with
//! [`FrameGate::post_encode_drop`], after which deltas already encoded are cancelled too until
//! the forced IDR. Outputs are matched to inputs by sample time (there are no B-frames, so they
//! come out in order; inputs older than an output were dropped by the encoder's rate control).
//!
//! An MFT that keeps failing (several frames in a row, an error event, or no output for
//! [`STUCK_AFTER`]) is rebuilt by [`H264Encoder::maintain`] with backoff; one that cannot be
//! brought back is reported by [`H264Encoder::fatal`].
//!
//! Thread model (async MFTs): the capture thread calls `encode_*`, which queues the sample and
//! feeds the MFT as far as its `METransformNeedInput` events allow; an event thread blocks in
//! `IMFMediaEventGenerator::GetEvent` and handles `NeedInput` / `HaveOutput`. Every MFT call
//! happens under one mutex.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use bytes::Bytes;
use parking_lot::{Condvar, Mutex};
use rd_agent_core::h264::{annexb_nals, annexb_to_avcc, build_avcc, codec_string_from_avcc, vui};
use rd_agent_core::{CodecConfig, EncodedFrame, FrameGate, FrameSink};
use tracing::{debug, error, info, warn};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_CPU_ACCESS_READ,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFShutdown, IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    MEError, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NOTACCEPTING, MF_E_SHUTDOWN, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_TYPE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VT_BOOL, VT_UI4};
use windows::core::{GUID, HRESULT, Interface as _};

use crate::d3d::Gpu;
use crate::mf;

/// Rebuild the MFT after this many frames in a row failed.
const FAILURES_BEFORE_REBUILD: u32 = 3;
/// Rebuild the MFT when it has held a frame this long without any output.
pub const STUCK_AFTER: Duration = Duration::from_secs(2);
/// Rebuilds are at least this far apart; the gap doubles after each one up to the maximum,
/// and resets once the MFT delivers [`HEALTHY_AFTER_FRAMES`].
const REBUILD_BACKOFF_MIN: Duration = Duration::from_secs(1);
const REBUILD_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Rebuild attempts in a row (without a healthy MFT in between) before the encoder is
/// reported as failed.
const REBUILDS_BEFORE_FATAL: u32 = 3;
/// Delivered frames after which a (re)built MFT counts as healthy.
const HEALTHY_AFTER_FRAMES: u64 = 10;
/// Input surfaces in the pool. A surface is busy from `encode_surface` until the MFT has
/// produced (or dropped) its frame; the flow window keeps far fewer frames in flight.
const POOL_SIZE: usize = 8;
/// Samples held for an async MFT that has not asked for input yet. More means the encoder is
/// not keeping up; the oldest is then dropped.
const MAX_QUEUED: usize = 2;
/// How long stopping waits for the MFT to hand out its last frames.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Bitrate used when the runtime has not set one yet (some MFTs require a non-zero rate).
const DEFAULT_BITRATE: u32 = 8_000_000;

/// Environment override for the encoder choice: `auto` (default), `hardware` or `software`.
pub const ENCODER_ENV: &str = "RD_MF_ENCODER";

/// True for the `n`th occurrence of something worth a log line: 1, 2, 4, 8, ...
fn log_nth(n: u64) -> bool {
    n.is_power_of_two()
}

/// Which encoder MFTs may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderPreference {
    /// The adapter's hardware encoder, else the software one.
    #[default]
    Auto,
    Hardware,
    Software,
}

impl EncoderPreference {
    /// The default, unless overridden with `RD_MF_ENCODER=auto|hardware|software`.
    pub fn from_env() -> Self {
        match std::env::var(ENCODER_ENV) {
            Err(_) => EncoderPreference::Auto,
            Ok(v) => v.parse().unwrap_or_else(|e| {
                warn!("{ENCODER_ENV}: {e:#}; using auto");
                EncoderPreference::Auto
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EncoderPreference::Auto => "auto",
            EncoderPreference::Hardware => "hardware",
            EncoderPreference::Software => "software",
        }
    }
}

impl std::str::FromStr for EncoderPreference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "auto" => Ok(EncoderPreference::Auto),
            "hardware" => Ok(EncoderPreference::Hardware),
            "software" => Ok(EncoderPreference::Software),
            _ => bail!("unknown encoder {s:?} (expected auto, hardware or software)"),
        }
    }
}

/// Encoder parameters fixed for the lifetime of an MFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preference: EncoderPreference,
}

/// Bitrate target shared between a pipeline and its (possibly replaced) encoders. Written by
/// `set_bitrate` from any thread; applied before the next frame.
pub type BitrateTarget = Arc<AtomicU32>;

/// An NV12 frame in system memory: `height` rows of luma, then `height / 2` rows of
/// interleaved CbCr, all `width` bytes long.
#[derive(Debug, Clone)]
pub struct Nv12Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Nv12Image {
    pub fn new(width: u32, height: u32) -> Self {
        let len = width as usize * height as usize * 3 / 2;
        Nv12Image {
            width,
            height,
            data: vec![0; len],
        }
    }

    /// Luma and chroma planes for drawing.
    pub fn planes(&mut self) -> rd_agent_core::pattern::Nv12Planes<'_> {
        let w = self.width as usize;
        let (luma, chroma) = self.data.split_at_mut(w * self.height as usize);
        rd_agent_core::pattern::Nv12Planes {
            luma,
            luma_stride: w,
            chroma,
            chroma_stride: w,
        }
    }
}

/// A pool texture to render the next frame into, from [`H264Encoder::acquire_surface`].
pub struct Surface {
    index: usize,
    /// The session whose pool it belongs to (a rebuild replaces the pool).
    owner: *const Inner,
    texture: ID3D11Texture2D,
}

// SAFETY: `owner` is only compared, never dereferenced; textures are free-threaded.
unsafe impl Send for Surface {}

impl Surface {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }
}

/// The H.264 encoder of a pipeline: an MFT plus what survives a rebuild.
pub struct H264Encoder {
    session: Session,
    cfg: EncoderConfig,
    gpu: Option<Gpu>,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    bitrate: BitrateTarget,
    last_pts_us: Option<u64>,
    recovery: Recovery,
}

impl H264Encoder {
    /// Opens an encoder. With a `gpu`, frames come as textures on that device (see
    /// [`Self::acquire_surface`]) and its adapter's hardware encoder is preferred; without
    /// one only [`Self::encode_image`] can be used and only software encoders are tried.
    pub fn new(
        cfg: EncoderConfig,
        gpu: Option<Gpu>,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
    ) -> anyhow::Result<Self> {
        ensure!(
            cfg.width >= 64
                && cfg.height >= 64
                && cfg.width.is_multiple_of(2)
                && cfg.height.is_multiple_of(2)
                && cfg.fps > 0,
            "invalid encoder config {cfg:?}"
        );
        mf::ensure_started()?;
        let session = Session::open(&cfg, gpu.as_ref(), &gate, &sink, &bitrate)?;
        Ok(H264Encoder {
            session,
            cfg,
            gpu,
            gate,
            sink,
            bitrate,
            last_pts_us: None,
            recovery: Recovery::default(),
        })
    }

    /// e.g. "NVIDIA H.264 Encoder MFT".
    pub fn name(&self) -> &str {
        &self.session.inner.name
    }

    pub fn hardware(&self) -> bool {
        self.session.inner.hardware
    }

    /// The bitrate the MFT was last configured with (bits/s).
    pub fn applied_bitrate(&self) -> u32 {
        self.session.inner.state.lock().applied_bitrate
    }

    /// A forced keyframe produced no output, so the next frame will be forced instead. On a
    /// static screen there may be no next frame: the pipeline then re-submits its last one.
    pub fn keyframe_pending(&self) -> bool {
        self.session.inner.force_next.load(Ordering::Relaxed)
    }

    /// Why the encoder is considered dead (rebuilding keeps failing), if it is.
    pub fn fatal(&self) -> Option<&str> {
        self.recovery.fatal.as_deref()
    }

    /// A free pool texture to render the next frame into, on the encoder's device. `None` if
    /// the encoder has no device or every surface is still being encoded (drop the frame).
    pub fn acquire_surface(&self) -> Option<Surface> {
        let inner = &self.session.inner;
        let mut state = inner.state.lock();
        let index = state.pool.iter().position(|s| !s.busy)?;
        let slot = &mut state.pool[index];
        slot.busy = true;
        Some(Surface {
            index,
            owner: Arc::as_ptr(inner),
            texture: slot.texture.clone(),
        })
    }

    /// Hands back a surface that will not be encoded after all.
    pub fn release_surface(&self, surface: Surface) {
        self.session.inner.free_surface(self.pool_index(&surface));
    }

    /// The surface's index in the current pool; `None` for one from before a rebuild.
    fn pool_index(&self, surface: &Surface) -> Option<usize> {
        (surface.owner == Arc::as_ptr(&self.session.inner)).then_some(surface.index)
    }

    /// Rebuilds the MFT if it has failed persistently (see the module docs). The new one starts
    /// with an IDR and a new `CodecConfig` (a new epoch). Pipelines call it before every frame
    /// (before [`Self::acquire_surface`]), also ones the gate refuses: a stuck MFT keeps the
    /// window full, so then nothing is admitted.
    pub fn maintain(&mut self, now: Instant) {
        let inner = &self.session.inner;
        let Some(reason) = inner.unhealthy(now) else {
            if inner.stats.delivered.load(Ordering::Relaxed) >= HEALTHY_AFTER_FRAMES {
                self.recovery.healthy();
            }
            return;
        };
        if !self.recovery.may_attempt(now) {
            return;
        }
        let attempt = self.recovery.attempts + 1;
        warn!(%reason, attempt, "rebuilding the H.264 encoder");
        match Session::open(
            &self.cfg,
            self.gpu.as_ref(),
            &self.gate,
            &self.sink,
            &self.bitrate,
        ) {
            // The old session is drained and shut down when it is replaced; its pending
            // frames are cancelled in the gate.
            Ok(fresh) => drop(std::mem::replace(&mut self.session, fresh)),
            Err(e) => error!(attempt, "rebuilding the H.264 encoder failed: {e:#}"),
        }
        self.recovery.attempted(now, &reason);
        if let Some(fatal) = &self.recovery.fatal
            && self.recovery.attempts == REBUILDS_BEFORE_FATAL
        {
            error!("{fatal}");
        }
    }

    /// Encodes the frame rendered into `surface`. The result arrives on the [`FrameSink`];
    /// on failure `seq` has already been released in the gate.
    pub fn encode_surface(
        &mut self,
        surface: Surface,
        capture_us: u64,
        seq: u32,
        force_key: bool,
    ) -> anyhow::Result<()> {
        let inner = self.session.inner.clone();
        let index = self.pool_index(&surface);
        let frame = self.frame(capture_us, seq, force_key);
        let sample = if inner.hardware {
            texture_sample(&surface.texture)
        } else {
            // The software MFT takes system memory: read the frame back. The surface is free
            // again as soon as it has been copied.
            let image = match &self.gpu {
                Some(gpu) => inner.readback(gpu, &surface.texture, self.cfg),
                None => Err(anyhow::anyhow!("a surface without a GPU")),
            };
            inner.free_surface(index);
            image.and_then(|image| memory_sample(&image))
        };
        let tracked = if inner.hardware { index } else { None };
        self.submit(&inner, frame, tracked, sample)
    }

    /// Encodes a frame from system memory (uploaded to a surface for hardware encoders).
    pub fn encode_image(
        &mut self,
        image: &Nv12Image,
        capture_us: u64,
        seq: u32,
        force_key: bool,
    ) -> anyhow::Result<()> {
        ensure!(
            (image.width, image.height) == (self.cfg.width, self.cfg.height),
            "image is {}x{}, the encoder {}x{}",
            image.width,
            image.height,
            self.cfg.width,
            self.cfg.height
        );
        self.maintain(Instant::now());
        if !self.session.inner.hardware {
            let frame = self.frame(capture_us, seq, force_key);
            let inner = self.session.inner.clone();
            return self.submit(&inner, frame, None, memory_sample(image));
        }
        let Some(surface) = self.acquire_surface() else {
            self.gate.cancel(seq);
            bail!("no free encoder surface");
        };
        let gpu = self
            .gpu
            .as_ref()
            .context("hardware encoder without a GPU")?;
        if let Err(e) = self.session.inner.upload(gpu, image, &surface.texture) {
            self.release_surface(surface);
            self.gate.cancel(seq);
            return Err(e);
        }
        self.encode_surface(surface, capture_us, seq, force_key)
    }

    fn frame(&mut self, capture_us: u64, seq: u32, force_key: bool) -> Pending {
        // Strictly increasing sample times: outputs are matched to inputs by time.
        let pts_us = match self.last_pts_us {
            Some(last) if capture_us <= last => last + 1,
            _ => capture_us,
        };
        self.last_pts_us = Some(pts_us);
        Pending {
            time: pts_us as i64 * 10,
            seq,
            capture_us,
            force_key,
            submitted: Instant::now(),
            surface: None,
        }
    }

    fn submit(
        &mut self,
        inner: &Arc<Inner>,
        mut frame: Pending,
        surface: Option<usize>,
        sample: anyhow::Result<IMFSample>,
    ) -> anyhow::Result<()> {
        frame.surface = surface;
        let duration = 10_000_000 / i64::from(self.cfg.fps);
        let sample = sample.and_then(|s| {
            // SAFETY: valid sample; plain setters.
            unsafe {
                s.SetSampleTime(frame.time)?;
                s.SetSampleDuration(duration)?;
            }
            Ok(s)
        });
        let sample = match sample {
            Ok(s) => s,
            Err(e) => {
                inner.free_surface(surface);
                inner.release_unencoded(&frame);
                return Err(e.context("preparing the input sample"));
            }
        };
        let bitrate = self.bitrate.load(Ordering::Relaxed);
        inner.submit(frame, sample, bitrate)
    }
}

/// One MFT and its event thread.
struct Session {
    inner: Arc<Inner>,
    pump: Option<JoinHandle<()>>,
    activate: IMFActivate,
}

impl Session {
    /// The first encoder (in preference order) that accepts the configuration.
    fn open(
        cfg: &EncoderConfig,
        gpu: Option<&Gpu>,
        gate: &Arc<dyn FrameGate>,
        sink: &FrameSink,
        bitrate: &BitrateTarget,
    ) -> anyhow::Result<Self> {
        let hardware_gpu = gpu.filter(|g| !g.software);
        let candidates = mf::h264_encoders(hardware_gpu.map(|g| g.luid))?;
        let mut errors = Vec::new();
        for info in candidates {
            let wanted = match cfg.preference {
                EncoderPreference::Auto => !info.hardware || hardware_gpu.is_some(),
                EncoderPreference::Hardware => info.hardware && hardware_gpu.is_some(),
                EncoderPreference::Software => !info.hardware,
            };
            if !wanted {
                continue;
            }
            let start = bitrate.load(Ordering::Relaxed);
            match Inner::create(&info, cfg, gpu, gate.clone(), sink.clone(), start) {
                Ok((inner, events)) => {
                    let inner = Arc::new(inner);
                    let pump = match events {
                        Some(events) => {
                            let thread_inner = inner.clone();
                            let events = mf::Mta(events);
                            Some(
                                std::thread::Builder::new()
                                    .name("rd-encoder-events".into())
                                    .spawn(move || {
                                        // The whole wrapper, not just its (non-Send) field.
                                        let events = events;
                                        pump(&thread_inner, &events.0)
                                    })
                                    .context("spawning the encoder event thread")?,
                            )
                        }
                        None => None,
                    };
                    inner.start()?;
                    info!(
                        encoder = %inner.name,
                        hardware = inner.hardware,
                        width = cfg.width,
                        height = cfg.height,
                        fps = cfg.fps,
                        bitrate_bps = inner.state.lock().applied_bitrate,
                        "H.264 encoder opened"
                    );
                    return Ok(Session {
                        inner,
                        pump,
                        activate: info.activate,
                    });
                }
                Err(e) => {
                    debug!(encoder = %info.name, "cannot use encoder: {e:#}");
                    errors.push(format!("{}: {e:#}", info.name));
                    // SAFETY: valid activation object; releases whatever it created.
                    let _ = unsafe { info.activate.ShutdownObject() };
                }
            }
        }
        if errors.is_empty() {
            bail!(
                "no {} H.264 encoder available",
                match cfg.preference {
                    EncoderPreference::Hardware => "hardware",
                    EncoderPreference::Software => "software",
                    EncoderPreference::Auto => "usable",
                }
            );
        }
        bail!("no H.264 encoder accepted {cfg:?}: {}", errors.join("; "))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.drain();
        // SAFETY: valid MFT; afterwards GetEvent returns MF_E_SHUTDOWN, ending the pump.
        let shut_down = inner
            .mft
            .cast::<IMFShutdown>()
            .and_then(|s| unsafe { s.Shutdown() })
            .inspect_err(|e| debug!("IMFShutdown::Shutdown: {e}"))
            .is_ok();
        if let Some(pump) = self.pump.take() {
            if !shut_down {
                // Async MFTs must implement IMFShutdown; without it the event thread could
                // block in GetEvent forever, so it is left behind rather than joined.
                warn!(encoder = %inner.name, "cannot shut the encoder down; leaving its event thread");
            } else if pump.join().is_err() {
                error!("encoder event thread panicked");
            }
        }
        let leftover: Vec<Pending> = inner.state.lock().pending.drain(..).collect();
        for frame in &leftover {
            inner.gate.cancel(frame.seq);
        }
        // SAFETY: valid activation object; the MFT is no longer used.
        let _ = unsafe { self.activate.ShutdownObject() };
        let stats = &inner.stats;
        debug!(
            encoder = %inner.name,
            delivered = stats.delivered.load(Ordering::Relaxed),
            cancelled = stats.cancelled.load(Ordering::Relaxed) + leftover.len() as u64,
            failed = stats.failed.load(Ordering::Relaxed),
            post_encode_drops = stats.post_encode_drops.load(Ordering::Relaxed),
            chain_drops = stats.chain_drops.load(Ordering::Relaxed),
            "H.264 encoder closed"
        );
    }
}

// ---------------------------------------------------------------------------
// MFT state

struct Pending {
    /// Sample time, 100 ns units.
    time: i64,
    seq: u32,
    capture_us: u64,
    force_key: bool,
    submitted: Instant,
    /// Pool surface the frame was rendered into (hardware encoders), freed with the frame.
    surface: Option<usize>,
}

struct Queued {
    sample: IMFSample,
    time: i64,
    force_key: bool,
}

struct PoolSurface {
    texture: ID3D11Texture2D,
    busy: bool,
}

struct State {
    /// `METransformNeedInput` events not answered yet (async MFTs).
    need_input: u32,
    /// Samples waiting for a `NeedInput` (async MFTs), oldest first.
    queued: VecDeque<Queued>,
    /// Frames handed to `submit` and not yet output or released, oldest first.
    pending: VecDeque<Pending>,
    pool: Vec<PoolSurface>,
    format: FormatState,
    applied_bitrate: u32,
    /// Readback / upload staging texture (software encoding from, or hardware encoding of
    /// system memory).
    staging: Option<ID3D11Texture2D>,
}

#[derive(Default)]
struct Stats {
    delivered: AtomicU64,
    cancelled: AtomicU64,
    failed: AtomicU64,
    post_encode_drops: AtomicU64,
    chain_drops: AtomicU64,
    package_errors: AtomicU64,
}

struct Inner {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    /// Keeps the D3D device manager alive for as long as the MFT uses it.
    _device_manager: Option<IMFDXGIDeviceManager>,
    name: String,
    hardware: bool,
    is_async: bool,
    /// The MFT expects us to allocate output samples of this size.
    output_alloc: Option<u32>,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    state: Mutex<State>,
    drained: (Mutex<bool>, Condvar),
    /// A forced keyframe produced no output: force the next frame instead.
    force_next: AtomicBool,
    /// An encoded frame was lost, so deltas behind it are cancelled until a keyframe has been
    /// delivered.
    chain_broken: AtomicBool,
    /// The MFT reported an error event; rebuild it.
    broken: AtomicBool,
    consecutive_failures: AtomicU32,
    last_failure: AtomicI32,
    stats: Stats,
}

// SAFETY: the MFT, its codec API, the device manager, samples and textures are MTA objects;
// every call on the MFT happens under `state`'s mutex, so they are never used concurrently.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                ..Default::default()
            }),
        },
    };
    // SAFETY: writing the member that `vt` selects.
    unsafe { (*var.Anonymous.Anonymous).Anonymous.ulVal = v };
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                ..Default::default()
            }),
        },
    };
    // SAFETY: writing the member that `vt` selects (VARIANT_TRUE is -1).
    unsafe {
        (*var.Anonymous.Anonymous).Anonymous.boolVal =
            windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    }
    var
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

impl Inner {
    /// Activates and configures an MFT. Returns the event generator of async MFTs.
    fn create(
        info: &mf::EncoderInfo,
        cfg: &EncoderConfig,
        gpu: Option<&Gpu>,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        start_bps: u32,
    ) -> anyhow::Result<(Inner, Option<IMFMediaEventGenerator>)> {
        // SAFETY (whole function): COM calls on objects created here, in the MTA.
        let mft: IMFTransform =
            unsafe { info.activate.ActivateObject() }.context("activating the MFT")?;
        let attrs = unsafe { mft.GetAttributes() }.ok();
        let is_async = attrs
            .as_ref()
            .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
            .unwrap_or(0)
            != 0;
        if let Some(a) = &attrs {
            if is_async {
                unsafe { a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                    .context("unlocking the async MFT")?;
            }
            let _ = unsafe { a.SetUINT32(&MF_LOW_LATENCY, 1) };
        }
        let device_manager = match gpu.filter(|_| info.hardware) {
            Some(gpu) => {
                let mut token = 0u32;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
                    .context("MFCreateDXGIDeviceManager")?;
                let manager = manager.context("no DXGI device manager")?;
                unsafe { manager.ResetDevice(&gpu.device, token) }
                    .context("IMFDXGIDeviceManager::ResetDevice")?;
                unsafe {
                    mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                }
                .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
                Some(manager)
            }
            None => None,
        };
        let codec = mft.cast::<ICodecAPI>().ok();
        let bitrate = if start_bps == 0 {
            DEFAULT_BITRATE
        } else {
            start_bps
        };
        if let Some(codec) = &codec {
            let set = |what: &str, api: &GUID, value: VARIANT| {
                // SAFETY: a VARIANT of the type the property documents.
                if let Err(e) = unsafe { codec.SetValue(api, &value) } {
                    debug!(encoder = %info.name, "{what}: {e}");
                }
            };
            set(
                "rate control CBR",
                &CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set(
                "low latency",
                &CODECAPI_AVLowLatencyMode,
                variant_bool(true),
            );
            set(
                "no B-frames",
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                variant_u32(0),
            );
            // IDRs on demand only: one per hour unless asked for.
            set(
                "GOP size",
                &CODECAPI_AVEncMPVGOPSize,
                variant_u32(cfg.fps.saturating_mul(3600)),
            );
            set(
                "quality vs speed",
                &CODECAPI_AVEncCommonQualityVsSpeed,
                variant_u32(33),
            );
            set(
                "mean bitrate",
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(bitrate),
            );
        }

        let output = unsafe { MFCreateMediaType() }?;
        unsafe {
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
        set_colour(&output);
        unsafe { mft.SetOutputType(0, &output, 0) }.context("setting the H.264 output type")?;

        let input = unsafe { MFCreateMediaType() }?;
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width)?;
        }
        set_colour(&input);
        unsafe { mft.SetInputType(0, &input, 0) }.context("setting the NV12 input type")?;

        let stream = unsafe { mft.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo")?;
        let provides =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        let output_alloc = (stream.dwFlags & provides == 0).then(|| {
            let raw = cfg.width * cfg.height * 3 / 2;
            stream.cbSize.max(raw / 2).max(64 * 1024)
        });
        let events = if is_async {
            Some(
                mft.cast::<IMFMediaEventGenerator>()
                    .context("async MFT without an event generator")?,
            )
        } else {
            None
        };
        let pool = match gpu {
            Some(gpu) => (0..POOL_SIZE)
                .map(|_| {
                    Ok(PoolSurface {
                        texture: nv12_texture(gpu, cfg.width, cfg.height)?,
                        busy: false,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        Ok((
            Inner {
                mft,
                codec,
                _device_manager: device_manager,
                name: info.name.clone(),
                hardware: info.hardware,
                is_async,
                output_alloc,
                gate,
                sink,
                state: Mutex::new(State {
                    need_input: 0,
                    queued: VecDeque::new(),
                    pending: VecDeque::new(),
                    pool,
                    format: FormatState::new(cfg.width, cfg.height),
                    applied_bitrate: bitrate,
                    staging: None,
                }),
                drained: (Mutex::new(false), Condvar::new()),
                force_next: AtomicBool::new(false),
                chain_broken: AtomicBool::new(false),
                broken: AtomicBool::new(false),
                consecutive_failures: AtomicU32::new(0),
                last_failure: AtomicI32::new(0),
                stats: Stats::default(),
            },
            events,
        ))
    }

    fn start(&self) -> anyhow::Result<()> {
        let _state = self.state.lock();
        // SAFETY: valid MFT, configured.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }
        Ok(())
    }

    /// Why the MFT should be rebuilt, if it should.
    fn unhealthy(&self, now: Instant) -> Option<String> {
        if self.broken.load(Ordering::Relaxed) {
            let last = self.last_failure.load(Ordering::Relaxed);
            return Some(format!("encoder error {:#010x}", last as u32));
        }
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        if failures >= FAILURES_BEFORE_REBUILD {
            let last = self.last_failure.load(Ordering::Relaxed);
            return Some(format!(
                "{failures} frames failed in a row (last error {:#010x})",
                last as u32
            ));
        }
        let oldest = self.state.lock().pending.front().map(|f| f.submitted)?;
        let waited = now.saturating_duration_since(oldest);
        (waited >= STUCK_AFTER).then(|| format!("no output for {} ms", waited.as_millis()))
    }

    fn failed(&self, hr: HRESULT, seq: Option<u32>, what: &str) {
        self.last_failure.store(hr.0, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let n = self.stats.failed.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
            warn!(encoder = %self.name, hr = format_args!("{:#010x}", hr.0 as u32), ?seq, total = n, "{what}");
        }
    }

    fn free_surface(&self, index: Option<usize>) {
        if let Some(i) = index {
            Self::free_in(&mut self.state.lock(), Some(i));
        }
    }

    fn free_in(state: &mut State, index: Option<usize>) {
        if let Some(slot) = index.and_then(|i| state.pool.get_mut(i)) {
            slot.busy = false;
        }
    }

    /// An admitted frame produced no output.
    fn release_unencoded(&self, frame: &Pending) {
        if frame.force_key {
            self.force_next.store(true, Ordering::Relaxed);
        }
        self.stats.cancelled.fetch_add(1, Ordering::Relaxed);
        self.gate.cancel(frame.seq);
    }

    /// An encoded frame was lost: the reference chain is broken until the next IDR, which the
    /// gate now forces.
    fn release_encoded(&self, seq: u32) {
        self.chain_broken.store(true, Ordering::Relaxed);
        let n = self.stats.post_encode_drops.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
            warn!(seq, total = n, "encoded frame dropped; forcing an IDR");
        }
        self.gate.post_encode_drop(seq);
    }

    fn submit(&self, frame: Pending, sample: IMFSample, bitrate: u32) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        self.apply_bitrate(&mut state, bitrate);
        let force_key = frame.force_key | self.force_next.swap(false, Ordering::Relaxed);
        let (time, seq) = (frame.time, frame.seq);
        state.pending.push_back(Pending { force_key, ..frame });
        if self.is_async {
            state.queued.push_back(Queued {
                sample,
                time,
                force_key,
            });
            while state.queued.len() > MAX_QUEUED {
                let Some(old) = state.queued.pop_front() else {
                    break;
                };
                debug!("the encoder is not asking for input; dropping the oldest queued frame");
                self.drop_pending(&mut state, old.time);
            }
            self.feed(&mut state);
            return Ok(());
        }
        // Synchronous MFT: in, then everything it has out.
        self.set_force_key(force_key);
        // SAFETY: valid MFT and sample.
        let mut result = unsafe { self.mft.ProcessInput(0, &sample, 0) };
        if result
            .as_ref()
            .is_err_and(|e| e.code() == MF_E_NOTACCEPTING)
        {
            self.drain_outputs(&mut state);
            // SAFETY: as above.
            result = unsafe { self.mft.ProcessInput(0, &sample, 0) };
        }
        if let Err(e) = result {
            self.failed(e.code(), Some(seq), "ProcessInput failed");
            self.drop_pending(&mut state, time);
            bail!("ProcessInput failed: {e}");
        }
        self.drain_outputs(&mut state);
        Ok(())
    }

    /// Removes the pending frame with sample time `time` and releases it unencoded.
    fn drop_pending(&self, state: &mut State, time: i64) {
        if let Some(i) = state.pending.iter().position(|p| p.time == time)
            && let Some(frame) = state.pending.remove(i)
        {
            Self::free_in(state, frame.surface);
            self.release_unencoded(&frame);
        }
    }

    fn set_force_key(&self, force: bool) {
        if !force {
            return;
        }
        if let Some(codec) = &self.codec {
            // SAFETY: CODECAPI_AVEncVideoForceKeyFrame takes a VT_UI4.
            if let Err(e) =
                unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
            {
                debug!("forcing a keyframe: {e}");
            }
        }
    }

    fn apply_bitrate(&self, state: &mut State, want: u32) {
        if want == 0 || want == state.applied_bitrate {
            return;
        }
        if let Some(codec) = &self.codec {
            // SAFETY: CODECAPI_AVEncCommonMeanBitRate takes a VT_UI4.
            if let Err(e) =
                unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(want)) }
            {
                debug!(want, "setting the bitrate: {e}");
            }
        }
        // Even on failure: retrying every frame would only spam the log.
        state.applied_bitrate = want;
    }

    /// Async MFTs: answer outstanding `NeedInput` events with queued samples.
    fn feed(&self, state: &mut State) {
        while state.need_input > 0 {
            let Some(q) = state.queued.pop_front() else {
                return;
            };
            state.need_input -= 1;
            self.set_force_key(q.force_key);
            // SAFETY: valid MFT and sample; the MFT asked for input.
            if let Err(e) = unsafe { self.mft.ProcessInput(0, &q.sample, 0) } {
                self.failed(e.code(), None, "ProcessInput failed");
                self.drop_pending(state, q.time);
            }
        }
    }

    fn on_need_input(&self) {
        let mut state = self.state.lock();
        state.need_input += 1;
        self.feed(&mut state);
    }

    fn on_have_output(&self) {
        let mut state = self.state.lock();
        self.process_output(&mut state);
    }

    /// Synchronous MFTs: pull outputs until the MFT wants more input.
    fn drain_outputs(&self, state: &mut State) {
        while self.process_output(state) {}
    }

    /// One `ProcessOutput`. Returns true if it produced a sample.
    fn process_output(&self, state: &mut State) -> bool {
        let sample = match self.output_alloc.map(alloc_sample).transpose() {
            Ok(s) => s,
            Err(e) => {
                warn!("allocating an output sample: {e:#}");
                return false;
            }
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: one output buffer for stream 0; we own whatever it holds afterwards.
        let result = unsafe {
            self.mft
                .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
        };
        // SAFETY: taken exactly once each.
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });
        match result {
            Ok(()) => match sample {
                Some(sample) => {
                    self.on_output(state, &sample);
                    true
                }
                None => false,
            },
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => false,
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The MFT changed its output type (e.g. new parameter sets): take its proposal.
                // SAFETY: valid MFT.
                let renegotiated = unsafe { self.mft.GetOutputAvailableType(0, 0) }
                    .and_then(|t| unsafe { self.mft.SetOutputType(0, &t, 0) });
                if let Err(e) = renegotiated {
                    self.failed(e.code(), None, "renegotiating the output type failed");
                    self.broken.store(true, Ordering::Relaxed);
                    return false;
                }
                info!(encoder = %self.name, "encoder output type changed");
                // Synchronous MFTs: try again; async ones send another HaveOutput.
                !self.is_async
            }
            Err(e) => {
                self.failed(e.code(), None, "ProcessOutput failed");
                false
            }
        }
    }

    fn on_output(&self, state: &mut State, sample: &IMFSample) {
        // SAFETY: valid sample.
        let time = unsafe { sample.GetSampleTime() }.ok();
        // Inputs older than this output were dropped by the encoder.
        let frame = loop {
            let Some(front) = state.pending.front() else {
                break None;
            };
            match time {
                Some(t) if front.time < t => {
                    let dropped = state.pending.pop_front().expect("front exists");
                    Self::free_in(state, dropped.surface);
                    debug!(seq = dropped.seq, "the encoder dropped a frame");
                    self.release_unencoded(&dropped);
                }
                Some(t) if front.time > t => break None,
                _ => break state.pending.pop_front(),
            }
        };
        let Some(frame) = frame else {
            debug!(?time, "output for an unknown frame; ignored");
            return;
        };
        Self::free_in(state, frame.surface);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let encode_us = u32::try_from(frame.submitted.elapsed().as_micros()).unwrap_or(u32::MAX);
        let annexb = match copy_sample(sample) {
            Ok(data) => data,
            Err(e) => {
                self.package_error(frame.seq, &e);
                self.release_encoded(frame.seq);
                return;
            }
        };
        // SAFETY: valid sample.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.is_ok_and(|v| v != 0);
        let au = AccessUnit::parse(&annexb);
        let key = au.idr || (clean_point && au.sps.is_some());
        if !self.chain_allows(frame.seq, key) {
            return;
        }
        let config = match state.format.config_for(&au, || self.sequence_header()) {
            Ok(config) => config,
            Err(e) => {
                self.package_error(frame.seq, &e);
                self.release_encoded(frame.seq);
                return;
            }
        };
        let carries_config = config.is_some();
        let data = annexb_to_avcc(&annexb);
        let packaged = if data.is_empty() {
            Err(anyhow::anyhow!("encoded sample has no slice data"))
        } else {
            Ok(EncodedFrame {
                seq: frame.seq,
                capture_us: frame.capture_us,
                encode_us,
                key,
                data: Bytes::from(data),
                config,
            })
        };
        self.deliver(state, frame.seq, key, carries_config, packaged);
    }

    /// SPS + PPS (Annex B) from the current output type, for MFTs that do not repeat them in
    /// band.
    fn sequence_header(&self) -> Option<Vec<u8>> {
        // SAFETY: valid MFT; the blob is copied out.
        unsafe {
            let ty: IMFMediaType = self.mft.GetOutputCurrentType(0).ok()?;
            let size = ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut buf = vec![0u8; size as usize];
            ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
                .ok()?;
            Some(buf)
        }
    }

    /// False (and `seq` is released) for a delta encoded before a lost frame was noticed: it
    /// references that frame, so the viewer could not decode it.
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
        state: &mut State,
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
            state.format.resend = true;
        }
        self.release_encoded(seq);
    }

    fn package_error(&self, seq: u32, e: &anyhow::Error) {
        let n = self.stats.package_errors.fetch_add(1, Ordering::Relaxed) + 1;
        if log_nth(n) {
            error!(seq, total = n, "cannot package encoded frame: {e:#}");
        }
    }

    /// End of stream: let the MFT hand out what it still holds (delivered as usual).
    fn drain(&self) {
        {
            let mut state = self.state.lock();
            // SAFETY: valid MFT.
            unsafe {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                if self
                    .mft
                    .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                    .is_err()
                {
                    return;
                }
            }
            if !self.is_async {
                self.drain_outputs(&mut state);
                // SAFETY: valid MFT.
                let _ = unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) };
                return;
            }
            // Samples that never got a NeedInput will not be encoded.
            let queued: Vec<i64> = state.queued.drain(..).map(|q| q.time).collect();
            for time in queued {
                self.drop_pending(&mut state, time);
            }
        }
        let (lock, cvar) = &self.drained;
        let mut drained = lock.lock();
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while !*drained {
            if cvar.wait_until(&mut drained, deadline).timed_out() {
                debug!(encoder = %self.name, "the encoder did not finish draining in time");
                break;
            }
        }
    }

    fn on_drained(&self) {
        let (lock, cvar) = &self.drained;
        *lock.lock() = true;
        cvar.notify_all();
    }

    /// Copies `texture` (NV12 on `gpu`) into system memory.
    fn readback(
        &self,
        gpu: &Gpu,
        texture: &ID3D11Texture2D,
        cfg: EncoderConfig,
    ) -> anyhow::Result<Nv12Image> {
        let mut state = self.state.lock();
        let staging = staging_texture(&mut state.staging, gpu, cfg.width, cfg.height, true)?;
        let mut image = Nv12Image::new(cfg.width, cfg.height);
        // SAFETY: both textures are NV12 of the same size on `gpu`; the mapping is read within
        // its bounds (RowPitch per row, the chroma plane after `height` luma rows).
        unsafe {
            gpu.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("mapping the readback texture")?;
            let (w, h) = (cfg.width as usize, cfg.height as usize);
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData.cast::<u8>();
            for row in 0..h + h / 2 {
                let from = std::slice::from_raw_parts(src.add(row * pitch), w);
                image.data[row * w..(row + 1) * w].copy_from_slice(from);
            }
            gpu.context.Unmap(&staging, 0);
        }
        Ok(image)
    }

    /// Copies `image` into `texture` (NV12 on `gpu`).
    fn upload(
        &self,
        gpu: &Gpu,
        image: &Nv12Image,
        texture: &ID3D11Texture2D,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        let staging = staging_texture(&mut state.staging, gpu, image.width, image.height, false)?;
        // SAFETY: as in `readback`, writing instead of reading.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .context("mapping the upload texture")?;
            let (w, h) = (image.width as usize, image.height as usize);
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData.cast::<u8>();
            for row in 0..h + h / 2 {
                let to = std::slice::from_raw_parts_mut(dst.add(row * pitch), w);
                to.copy_from_slice(&image.data[row * w..(row + 1) * w]);
            }
            gpu.context.Unmap(&staging, 0);
            gpu.context.CopyResource(texture, &staging);
        }
        Ok(())
    }
}

/// Async MFT event loop; ends when the MFT is shut down.
fn pump(inner: &Inner, events: &IMFMediaEventGenerator) {
    let _com = mf::ComThread::init();
    loop {
        // SAFETY: valid event generator; blocks until an event or shutdown.
        let event = match unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
            Ok(event) => event,
            Err(e) if e.code() == MF_E_SHUTDOWN => break,
            Err(e) => {
                inner.failed(e.code(), None, "IMFMediaEventGenerator::GetEvent failed");
                inner.broken.store(true, Ordering::Relaxed);
                inner.on_drained();
                break;
            }
        };
        // SAFETY: valid event.
        let ty = MF_EVENT_TYPE(unsafe { event.GetType() }.unwrap_or(0) as i32);
        let status = unsafe { event.GetStatus() }.unwrap_or(HRESULT(0));
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if ty == METransformNeedInput {
                inner.on_need_input();
            } else if ty == METransformHaveOutput {
                inner.on_have_output();
            } else if ty == METransformDrainComplete {
                inner.on_drained();
            } else if ty == MEError {
                inner.failed(status, None, "the encoder reported an error");
                inner.broken.store(true, Ordering::Relaxed);
            } else {
                debug!(event = ty.0, "encoder event");
            }
        }));
        if run.is_err() {
            error!("panic in the encoder event thread");
            inner.broken.store(true, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// helpers

fn set_colour(ty: &IMFMediaType) {
    // SAFETY: valid media type; plain attribute setters. Failures only lose the tags.
    unsafe {
        let _ = ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = ty.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        let _ = ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    }
}

/// A default-usage NV12 texture the video processor can render into and the encoder can read.
pub fn nv12_texture(gpu: &Gpu, width: u32, height: u32) -> anyhow::Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid description; out pointer to a local.
    let created = unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) };
    if created.is_err() {
        // Not every driver knows the video-encoder bind flag.
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        // SAFETY: as above.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .with_context(|| format!("creating a {width}x{height} NV12 texture"))?;
    }
    texture.context("CreateTexture2D returned no texture")
}

/// The CPU-readable (`read`) or -writable NV12 staging texture in `slot`, created on first use.
fn staging_texture(
    slot: &mut Option<ID3D11Texture2D>,
    gpu: &Gpu,
    width: u32,
    height: u32,
    read: bool,
) -> anyhow::Result<ID3D11Texture2D> {
    if let Some(t) = slot {
        return Ok(t.clone());
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: if read {
            D3D11_CPU_ACCESS_READ.0 as u32
        } else {
            D3D11_CPU_ACCESS_WRITE.0 as u32
        },
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid description; out pointer to a local.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("creating an NV12 staging texture")?;
    let texture = texture.context("CreateTexture2D returned no texture")?;
    *slot = Some(texture.clone());
    Ok(texture)
}

fn texture_sample(texture: &ID3D11Texture2D) -> anyhow::Result<IMFSample> {
    // SAFETY: a valid texture; the buffer holds a reference to it.
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
            .context("MFCreateDXGISurfaceBuffer")?;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>()
            && let Ok(len) = two_d.GetContiguousLength()
        {
            buffer.SetCurrentLength(len)?;
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

fn memory_sample(image: &Nv12Image) -> anyhow::Result<IMFSample> {
    let len = u32::try_from(image.data.len()).context("frame too large")?;
    // SAFETY: the locked buffer has room for `len` bytes.
    unsafe {
        let buffer = MFCreateMemoryBuffer(len)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(image.data.as_ptr(), ptr, image.data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

fn alloc_sample(size: u32) -> anyhow::Result<IMFSample> {
    // SAFETY: plain object creation.
    unsafe {
        let buffer = MFCreateMemoryBuffer(size)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

fn copy_sample(sample: &IMFSample) -> anyhow::Result<Vec<u8>> {
    // SAFETY: the locked range is `len` bytes long and copied before unlocking.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        ensure!(!data.is_empty(), "empty encoded sample");
        Ok(data)
    }
}

/// What an Annex-B access unit carries.
#[derive(Debug, Default, PartialEq, Eq)]
struct AccessUnit<'a> {
    sps: Option<&'a [u8]>,
    pps: Option<&'a [u8]>,
    idr: bool,
}

impl<'a> AccessUnit<'a> {
    fn parse(annexb: &'a [u8]) -> Self {
        let mut au = AccessUnit::default();
        for nal in annexb_nals(annexb) {
            match nal[0] & 0x1f {
                5 => au.idr = true,
                7 if au.sps.is_none() => au.sps = Some(nal),
                8 if au.pps.is_none() => au.pps = Some(nal),
                _ => {}
            }
        }
        au
    }
}

/// The current parameter sets and whether the next frame must carry them.
struct FormatState {
    width: u32,
    height: u32,
    sps: Vec<u8>,
    pps: Vec<u8>,
    config: Option<CodecConfig>,
    /// Attach `config` to the next delivered frame.
    resend: bool,
}

impl FormatState {
    fn new(width: u32, height: u32) -> Self {
        FormatState {
            width,
            height,
            sps: Vec::new(),
            pps: Vec::new(),
            config: None,
            resend: false,
        }
    }

    /// The `CodecConfig` to attach to this frame: `Some` for the first frame, when the
    /// parameter sets change, or after a frame carrying the config was lost. Parameter sets
    /// come from the access unit, else (first frame only) from `header`.
    fn config_for(
        &mut self,
        au: &AccessUnit<'_>,
        header: impl FnOnce() -> Option<Vec<u8>>,
    ) -> anyhow::Result<Option<CodecConfig>> {
        let fallback;
        let (sps, pps) = match (au.sps, au.pps) {
            (Some(sps), Some(pps)) => (sps, pps),
            _ if self.config.is_some() => return Ok(self.take_resend()),
            _ => {
                fallback = header().context("no SPS/PPS in the stream or the output type")?;
                let header = AccessUnit::parse(&fallback);
                match (header.sps, header.pps) {
                    (Some(sps), Some(pps)) => (sps, pps),
                    _ => bail!("the output type's sequence header has no SPS/PPS"),
                }
            }
        };
        if sps != self.sps.as_slice() || pps != self.pps.as_slice() {
            let config = codec_config(sps, pps, self.width, self.height)?;
            if self.config.as_ref() != Some(&config) {
                info!(codec = %config.codec, width = config.width, height = config.height,
                    avcc_len = config.avcc.len(), "encoder parameter sets");
                self.config = Some(config);
                self.resend = true;
            }
            self.sps = sps.to_vec();
            self.pps = pps.to_vec();
        }
        Ok(self.take_resend())
    }

    fn take_resend(&mut self) -> Option<CodecConfig> {
        if std::mem::take(&mut self.resend) {
            self.config.clone()
        } else {
            None
        }
    }
}

fn codec_config(sps: &[u8], pps: &[u8], width: u32, height: u32) -> anyhow::Result<CodecConfig> {
    let avcc = build_avcc(sps, pps).context("cannot build an avcC from the SPS/PPS")?;
    let avcc = match vui::without_reordering(&avcc) {
        Ok(Some(rewritten)) => {
            debug!("SPS VUI lacked max_num_reorder_frames = 0; added it to the avcC");
            Bytes::from(rewritten)
        }
        Ok(None) => avcc,
        Err(e) => {
            warn!("cannot inspect the SPS VUI: {e:#}");
            avcc
        }
    };
    let codec = codec_string_from_avcc(&avcc).context("malformed avcC record")?;
    Ok(CodecConfig {
        codec,
        width,
        height,
        avcc,
    })
}

/// Rebuild bookkeeping that outlives a single MFT.
#[derive(Debug, Default)]
struct Recovery {
    /// Rebuild attempts since the encoder was last healthy.
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
            info!(rebuilds = self.attempts, "H.264 encoder healthy again");
        }
        *self = Recovery::default();
    }
}

#[cfg(test)]
mod tests {
    use rd_agent_core::Admitted;
    use rd_agent_core::h264::{avcc_nals, avcc_sps_list};
    use rd_agent_core::pattern::TestPattern;
    use tokio::sync::mpsc;

    use super::*;

    /// Admits everything; forces an IDR on request or after a post-encode drop.
    #[derive(Default)]
    struct TestGate {
        next: AtomicU32,
        force: AtomicBool,
        cancelled: AtomicU64,
        post_drops: AtomicU64,
    }

    impl FrameGate for TestGate {
        fn admit(&self, _capture_us: u64) -> Option<Admitted> {
            Some(Admitted {
                seq: self.next.fetch_add(1, Ordering::Relaxed) + 1,
                force_keyframe: self.force.swap(false, Ordering::Relaxed),
            })
        }
        fn cancel(&self, _seq: u32) {
            self.cancelled.fetch_add(1, Ordering::Relaxed);
        }
        fn post_encode_drop(&self, _seq: u32) {
            self.post_drops.fetch_add(1, Ordering::Relaxed);
            self.force.store(true, Ordering::Relaxed);
        }
    }

    fn recv(rx: &mut mpsc::Receiver<EncodedFrame>) -> EncodedFrame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(frame) => return frame,
                Err(mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("no frame: {e:?}"),
            }
        }
    }

    /// 4-byte length-prefixed NAL units and nothing else, no parameter sets.
    fn valid_avcc(data: &[u8]) -> bool {
        let mut consumed = 0;
        for nal in avcc_nals(data) {
            if nal.is_empty() || matches!(nal[0] & 0x1f, 7 | 8) {
                return false;
            }
            consumed += 4 + nal.len();
        }
        consumed == data.len() && consumed > 0
    }

    /// Encodes the test pattern through the given encoder kind and checks what the runtime
    /// relies on. Returns false (skipped) when there is no such encoder here.
    fn round_trip(preference: EncoderPreference, gpu: bool) -> bool {
        let (w, h) = (320, 192);
        let gpu = if gpu {
            match crate::d3d::create_device(None) {
                Ok(g) => Some(g),
                Err(_) => return false,
            }
        } else {
            None
        };
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let bitrate: BitrateTarget = Arc::new(AtomicU32::new(2_000_000));
        let cfg = EncoderConfig {
            width: w,
            height: h,
            fps: 60,
            preference,
        };
        let mut encoder = match H264Encoder::new(cfg, gpu, gate.clone(), tx, bitrate.clone()) {
            Ok(e) => e,
            Err(e) => {
                println!("skipping {preference:?}: {e:#}");
                return false;
            }
        };
        println!("{} (hardware {})", encoder.name(), encoder.hardware());
        let mut pattern = TestPattern::new(w, h).unwrap();
        let mut image = Nv12Image::new(w, h);
        let mut send = |encoder: &mut H264Encoder, n: u64| {
            let t = rd_agent_core::clock::now_us();
            pattern.draw(&mut image.planes(), n, t);
            let a = gate.admit(t).unwrap();
            encoder
                .encode_image(&image, t, a.seq, a.force_keyframe)
                .unwrap();
            std::thread::sleep(Duration::from_millis(5));
        };
        for n in 0..10 {
            send(&mut encoder, n);
        }
        let first = recv(&mut rx);
        assert!(first.key, "the first frame is an IDR");
        let config = first
            .config
            .clone()
            .expect("the first frame carries the config");
        println!("{} avcC {} bytes", config.codec, config.avcc.len());
        assert!(config.codec.starts_with("avc1."), "{}", config.codec);
        assert_eq!((config.width, config.height), (w, h));
        assert_eq!(avcc_sps_list(&config.avcc).len(), 1);
        assert!(valid_avcc(&first.data));
        let mut last_seq = first.seq;
        for _ in 0..5 {
            let f = recv(&mut rx);
            assert!(f.seq > last_seq, "in order");
            last_seq = f.seq;
            assert!(!f.key && f.config.is_none(), "deltas follow");
            assert!(valid_avcc(&f.data));
        }

        // A forced keyframe, same parameter sets.
        while rx.try_recv().is_ok() {}
        gate.force.store(true, Ordering::Relaxed);
        for n in 10..20 {
            send(&mut encoder, n);
        }
        let mut keyed = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if f.key {
                keyed.push(f);
            }
        }
        assert_eq!(keyed.len(), 1, "one forced IDR");
        assert!(keyed[0].config.is_none(), "same parameter sets");

        // A new bitrate reaches the MFT with the next frame.
        bitrate.store(1_000_000, Ordering::Relaxed);
        send(&mut encoder, 20);
        assert_eq!(encoder.applied_bitrate(), 1_000_000);
        drop(encoder);
        while rx.try_recv().is_ok() {}
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert_eq!(gate.post_drops.load(Ordering::Relaxed), 0);
        true
    }

    #[test]
    fn software_encoder_round_trip() {
        assert!(
            round_trip(EncoderPreference::Software, false),
            "Microsoft's software H.264 encoder ships with Windows"
        );
    }

    #[test]
    fn software_encoder_reads_back_surfaces() {
        round_trip(EncoderPreference::Software, true);
    }

    #[test]
    fn hardware_encoder_round_trip() {
        // Skipped on machines (CI) without a hardware encoder.
        round_trip(EncoderPreference::Hardware, true);
    }

    #[test]
    fn access_units() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let parsed = AccessUnit::parse(&au);
        assert_eq!(parsed.sps, Some(&[0x67, 1, 2][..]));
        assert_eq!(parsed.pps, Some(&[0x68, 3][..]));
        assert!(parsed.idr);
        assert_eq!(annexb_to_avcc(&au), vec![0, 0, 0, 2, 0x65, 0x88]);
        let delta = AccessUnit::parse(&[0, 0, 1, 0x41, 0x9a]);
        assert_eq!(delta, AccessUnit::default());
    }

    #[test]
    fn preference_from_str() {
        assert_eq!(
            "software".parse::<EncoderPreference>().unwrap(),
            EncoderPreference::Software
        );
        assert!("gpu".parse::<EncoderPreference>().is_err());
    }
}
