//! Video pipelines: capture (or synthesize) frames, gate them, encode them with VideoToolbox.
//!
//! - [`ScreenPipeline`]: ScreenCaptureKit display capture (NV12 `420v`, scaled on the GPU).
//! - [`SyntheticPipeline`]: a moving NV12 test pattern through the same gate and encoder, for
//!   end-to-end tests without screen-recording permission.
//!
//! Both feed an `EncodeSlot`, which owns the encoder and keeps the most recent frame.
//! ScreenCaptureKit only delivers frames when the screen changes, so when the runtime wants a
//! keyframe ([`FrameGate::refresh_wanted`]) and nothing has arrived for [`REFRESH_AFTER`], the
//! slot re-submits that frame through the gate as usual. For screen capture the check runs on
//! the capture queue every [`REFRESH_TICK`], serialised with the sample callback.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use crossbeam_channel::RecvTimeoutError;
use dispatch2::{DispatchRetained, DispatchTime};
use objc2_core_foundation::{CFRetained, Type as _};
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayBounds, CGPreflightScreenCaptureAccess};
use objc2_core_video::CVPixelBuffer;
use parking_lot::Mutex;
use rd_agent_core::clock::now_us;
use rd_agent_core::{Admitted, FrameGate, FrameSink, VideoConfig, VideoPipeline};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::color_matrix;
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use tracing::{debug, error, info, warn};

use crate::encoder::{BitrateTarget, EncoderConfig, H264Encoder, RateControl};
use crate::geometry::{self, CaptureGeometry, CaptureLayout};
use crate::pattern::TestPattern;
use crate::session;

/// Frames ScreenCaptureKit may hold queued for us. One more than the encoder needs, because
/// the slot keeps the most recent frame retained for keyframe refreshes.
const SCK_QUEUE_DEPTH: u32 = 6;
/// A wanted keyframe is carried by the last frame once the source has been quiet this long.
/// Also the gap between attempts while the gate keeps refusing it.
pub const REFRESH_AFTER: Duration = Duration::from_millis(80);
/// How often the capture queue checks whether a refresh is due.
pub const REFRESH_TICK: Duration = Duration::from_millis(50);

/// Why a pipeline stopped working on its own; see [`VideoPipeline::failure`].
type Failure = Arc<Mutex<Option<String>>>;

fn record_failure(failure: &Mutex<Option<String>>, why: impl FnOnce() -> String) {
    let mut failure = failure.lock();
    if failure.is_none() {
        *failure = Some(why());
    }
}

fn encoder_config(cfg: &VideoConfig, rate_control: RateControl) -> EncoderConfig {
    EncoderConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        rate_control,
    }
}

fn validate(cfg: &VideoConfig) -> anyhow::Result<()> {
    ensure!(
        cfg.width >= 64
            && cfg.height >= 64
            && cfg.width.is_multiple_of(2)
            && cfg.height.is_multiple_of(2),
        "video size must be even and at least 64x64, got {}x{}",
        cfg.width,
        cfg.height
    );
    ensure!(
        (1..=240).contains(&cfg.fps),
        "unsupported frame rate {}",
        cfg.fps
    );
    Ok(())
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceIncrementUseCount(surface: NonNull<c_void>);
    fn IOSurfaceDecrementUseCount(surface: NonNull<c_void>);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    /// +0, owned by the pixel buffer; NULL if it is not IOSurface-backed.
    fn CVPixelBufferGetIOSurface(pixel_buffer: &CVPixelBuffer) -> Option<NonNull<c_void>>;
}

/// The slot's most recent frame: the pixel buffer retained, and its IOSurface marked in use,
/// so ScreenCaptureKit's capture service (another process, which only sees use counts) does
/// not draw a later frame into it while it may still be re-submitted. A live CVPixelBuffer
/// normally holds a use itself; this does not rely on how ScreenCaptureKit wraps its
/// surfaces (OBS's ScreenCaptureKit source holds its latest surface the same way).
struct Frame {
    buffer: CFRetained<CVPixelBuffer>,
    surface: Option<NonNull<c_void>>,
}

impl Frame {
    fn hold(buffer: CFRetained<CVPixelBuffer>) -> Self {
        // SAFETY: valid pixel buffer; the surface (if any) stays alive as long as `buffer`.
        let surface = unsafe { CVPixelBufferGetIOSurface(&buffer) };
        if let Some(surface) = surface {
            // SAFETY: a live IOSurface; balanced in `Drop`.
            unsafe { IOSurfaceIncrementUseCount(surface) };
        }
        Frame { buffer, surface }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(surface) = self.surface {
            // SAFETY: incremented in `hold`; `buffer` (released after this) keeps it alive.
            unsafe { IOSurfaceDecrementUseCount(surface) };
        }
    }
}

// SAFETY: CVPixelBuffer retain/release and IOSurface use counts are thread-safe. The buffer
// is only read (handed to VideoToolbox, which retains it for as long as it needs it), and only
// under the slot mutex.
unsafe impl Send for Frame {}

/// Gate + encoder + most recent frame, shared by the frame source and the refresh timer.
struct EncodeSlot {
    gate: Arc<dyn FrameGate>,
    state: Mutex<SlotState>,
    /// The pipeline's failure; set when the encoder cannot be brought back.
    failure: Failure,
}

struct SlotState {
    /// `None` once stopped. The lock is held for one admit + encode, which also orders a
    /// replaced encoder's final flush before the first frame of its successor.
    encoder: Option<H264Encoder>,
    /// The most recent frame (retained), re-submitted to carry a wanted keyframe when the
    /// source is quiet (static screen).
    last: Option<Frame>,
    /// When the source last produced a frame, or the slot last re-submitted one.
    last_submit: Option<Instant>,
}

impl EncodeSlot {
    fn new(gate: Arc<dyn FrameGate>, encoder: H264Encoder, failure: Failure) -> Self {
        EncodeSlot {
            gate,
            state: Mutex::new(SlotState {
                encoder: Some(encoder),
                last: None,
                last_submit: None,
            }),
            failure,
        }
    }

    /// New size or rate: the old encoder is flushed under the lock, before its successor (whose
    /// first frame carries the new CodecConfig) sees any input. The last frame has the old
    /// size, so it is released.
    fn replace_encoder(&self, encoder: H264Encoder) {
        let mut state = self.state.lock();
        state.last = None;
        drop(state.encoder.replace(encoder));
    }

    /// Flushes and releases the encoder and the last frame. Late callbacks and the refresh
    /// timer then find the slot empty (which ends the timer).
    fn shut_down(&self) {
        let mut state = self.state.lock();
        state.last = None;
        drop(state.encoder.take());
    }

    /// ScreenCaptureKit delivered a complete frame: keep it, then admit and encode it.
    fn on_captured(&self, image: &CVPixelBuffer, capture_us: u64) {
        let now = Instant::now();
        let mut state = self.state.lock();
        let SlotState {
            encoder,
            last,
            last_submit,
        } = &mut *state;
        let Some(encoder) = encoder.as_mut() else {
            return;
        };
        // Right after a resize ScreenCaptureKit may still deliver a few old-size frames.
        if !encoder.accepts(image) {
            return;
        }
        self.maintain(encoder, now);
        // Kept even if the gate refuses it: it is what the screen shows now.
        *last = Some(Frame::hold(image.retain()));
        *last_submit = Some(now);
        if let Some(admitted) = self.gate.admit(capture_us) {
            encode(encoder, image, capture_us, admitted);
        }
    }

    /// A synthetic frame is due: admit first, render only what is admitted.
    fn on_tick(
        &self,
        capture_us: u64,
        render: impl FnOnce() -> anyhow::Result<CFRetained<CVPixelBuffer>>,
    ) {
        let now = Instant::now();
        let mut state = self.state.lock();
        let SlotState {
            encoder,
            last,
            last_submit,
        } = &mut *state;
        let Some(encoder) = encoder.as_mut() else {
            return;
        };
        self.maintain(encoder, now);
        *last_submit = Some(now);
        let Some(admitted) = self.gate.admit(capture_us) else {
            return;
        };
        let image = match render() {
            Ok(image) => image,
            Err(e) => {
                warn!("rendering test pattern: {e:#}");
                self.gate.cancel(admitted.seq);
                return;
            }
        };
        encode(encoder, &image, capture_us, admitted);
        *last = Some(Frame::hold(image));
    }

    /// Re-submits the last frame if a keyframe is wanted (by the gate, or by the encoder after
    /// it dropped a forced one) and the source has been quiet for [`REFRESH_AFTER`]. Also lets
    /// the encoder recover while no frames arrive. Returns false once the slot has stopped.
    fn refresh_tick(&self, now: Instant) -> bool {
        let mut state = self.state.lock();
        let SlotState {
            encoder,
            last,
            last_submit,
        } = &mut *state;
        let Some(encoder) = encoder.as_mut() else {
            return false;
        };
        self.maintain(encoder, now);
        let quiet = last_submit.is_some_and(|t| now.saturating_duration_since(t) >= REFRESH_AFTER);
        let Some(frame) = last.as_ref().filter(|_| quiet) else {
            return true;
        };
        if !(self.gate.refresh_wanted() || encoder.keyframe_pending()) {
            return true;
        }
        if !encoder.accepts(&frame.buffer) {
            *last = None;
            return true;
        }
        *last_submit = Some(now);
        debug!("quiet source: re-submitting the last frame to carry a keyframe");
        // Stamped now: that is when it is shown, and capture-to-paint stats stay meaningful.
        let capture_us = now_us();
        if let Some(admitted) = self.gate.admit(capture_us) {
            encode(encoder, &frame.buffer, capture_us, admitted);
        }
        true
    }

    /// Lets the encoder rebuild a failing session; records it if that keeps failing.
    fn maintain(&self, encoder: &mut H264Encoder, now: Instant) {
        encoder.maintain(now);
        if let Some(fatal) = encoder.fatal() {
            record_failure(&self.failure, || fatal.to_owned());
        }
    }
}

/// Encodes an admitted frame. Failures are logged (rate-limited) and released by the encoder.
fn encode(encoder: &mut H264Encoder, image: &CVPixelBuffer, capture_us: u64, admitted: Admitted) {
    if let Err(e) = encoder.encode(
        image,
        capture_us,
        admitted.seq,
        capture_us,
        admitted.force_keyframe,
    ) {
        debug!(seq = admitted.seq, "encode failed: {e:#}");
    }
}

/// The capture queue as a `dispatch2` queue: both wrap the same `dispatch_queue_t`.
fn retained_queue(queue: &DispatchQueue) -> Option<DispatchRetained<dispatch2::DispatchQueue>> {
    let ptr = NonNull::new(queue.as_ptr().cast_mut().cast::<dispatch2::DispatchQueue>())?;
    // SAFETY: `ptr` is a live dispatch queue owned by `queue`; this takes a reference of our
    // own, released when the returned handle drops.
    Some(unsafe { DispatchRetained::retain(ptr) })
}

/// Runs [`EncodeSlot::refresh_tick`] on `queue` every [`REFRESH_TICK`] until the slot has been
/// shut down. On the capture queue this is serialised with the sample callback.
fn schedule_refresh(queue: DispatchRetained<dispatch2::DispatchQueue>, slot: Arc<EncodeSlot>) {
    let Ok(when) = DispatchTime::try_from(REFRESH_TICK) else {
        return;
    };
    let target = queue.clone();
    let scheduled = target.after(when, move || {
        // Never unwind into libdispatch.
        let running =
            std::panic::catch_unwind(AssertUnwindSafe(|| slot.refresh_tick(Instant::now())))
                .unwrap_or_else(|_| {
                    error!("panic in the capture refresh timer; keyframe refreshes stop");
                    false
                });
        if running {
            schedule_refresh(queue, slot);
        }
    });
    if scheduled.is_err() {
        warn!("cannot schedule the capture refresh timer");
    }
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit

/// Distinguishes pipelines publishing to the same [`CaptureGeometry`].
static NEXT_PIPELINE_ID: AtomicU64 = AtomicU64::new(1);

/// ScreenCaptureKit display capture → VideoToolbox.
pub struct ScreenPipeline {
    rate_control: RateControl,
    bitrate: BitrateTarget,
    /// Where the captured display sits in the frame, for the input injector.
    geometry: Option<Arc<CaptureGeometry>>,
    id: u64,
    running: Option<ScreenRun>,
}

struct ScreenRun {
    stream: SCStream,
    /// The serial capture queue; held for as long as the stream delivers on it.
    _queue: DispatchQueue,
    slot: Arc<EncodeSlot>,
    sink: FrameSink,
    cfg: VideoConfig,
    display_id: CGDirectDisplayID,
    /// Set by the stream delegate (capture stopped) or the slot (encoder dead).
    failure: Failure,
}

impl ScreenPipeline {
    /// Uses [`RateControl::from_env`].
    pub fn new() -> anyhow::Result<Self> {
        Self::with_rate_control(RateControl::from_env())
    }

    pub fn with_rate_control(rate_control: RateControl) -> anyhow::Result<Self> {
        Ok(ScreenPipeline {
            rate_control,
            bitrate: Arc::new(AtomicU32::new(0)),
            geometry: None,
            id: NEXT_PIPELINE_ID.fetch_add(1, Ordering::Relaxed),
            running: None,
        })
    }

    /// Publish the capture layout to `geometry` (read by the input injector).
    pub fn with_capture_geometry(mut self, geometry: Arc<CaptureGeometry>) -> Self {
        self.geometry = Some(geometry);
        self
    }

    fn new_encoder(
        &self,
        cfg: &VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<H264Encoder> {
        H264Encoder::new(
            encoder_config(cfg, self.rate_control),
            gate,
            sink,
            self.bitrate.clone(),
        )
    }

    fn publish(&self, layout: CaptureLayout) {
        if let Some(geometry) = &self.geometry {
            geometry.publish(self.id, layout);
        }
    }
}

/// How `display` will be laid out in a `cfg` frame (see [`geometry`]).
fn capture_layout(display: CGDirectDisplayID, cfg: &VideoConfig) -> CaptureLayout {
    let size = CGDisplayBounds(display).size;
    CaptureLayout {
        display,
        width: cfg.width,
        height: cfg.height,
        stretch: geometry::stretch_to_fill(size.width, size.height, cfg.width, cfg.height),
    }
}

impl VideoPipeline for ScreenPipeline {
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<()> {
        self.stop();
        validate(&cfg)?;
        // Never let ScreenCaptureKit trigger the permission prompt from the agent.
        ensure!(
            CGPreflightScreenCaptureAccess(),
            "Screen Recording permission is not granted (run `rd-agent setup` in person)"
        );
        self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);

        let display_id = session::resolve_display(cfg.display)
            .with_context(|| format!("display {} is not active", cfg.display))?;
        let content = SCShareableContent::get().context("listing shareable content")?;
        let display = content
            .displays()
            .into_iter()
            .find(|d| d.display_id() == display_id)
            .with_context(|| format!("ScreenCaptureKit does not list display {display_id}"))?;
        let pid = std::process::id() as i32;
        let own_apps: Vec<_> = content
            .applications()
            .into_iter()
            .filter(|a| a.process_id() == pid)
            .collect();
        let own_refs: Vec<&SCRunningApplication> = own_apps.iter().collect();
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_applications(&own_refs, &[])
            .build()
            .context("building content filter")?;

        let layout = capture_layout(display_id, &cfg);
        let failure = Failure::default();
        let encoder = self.new_encoder(&cfg, gate.clone(), sink.clone())?;
        let slot = Arc::new(EncodeSlot::new(gate, encoder, failure.clone()));
        let stopped = failure.clone();
        let delegate = StreamCallbacks::new().on_error(move |e| {
            error!("ScreenCaptureKit stream stopped: {e}");
            record_failure(&stopped, || format!("screen capture stopped: {e}"));
        });
        let mut stream = SCStream::new_with_delegate(
            &filter,
            &stream_configuration(&cfg, layout.stretch)?,
            delegate,
        )
        .context("creating SCStream")?;
        let queue = DispatchQueue::new("dev.thijmen.rd.capture", DispatchQoS::UserInteractive);
        let handler_slot = slot.clone();
        stream
            .add_output_handler_with_queue(
                move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                    if kind == SCStreamOutputType::Screen {
                        on_screen_sample(&handler_slot, &sample);
                    }
                },
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .context("adding stream output")?;
        stream.start_capture().context("starting capture")?;
        match retained_queue(&queue) {
            Some(timer_queue) => schedule_refresh(timer_queue, slot.clone()),
            None => warn!("no capture queue for the refresh timer; static screens get no IDR"),
        }
        self.publish(layout);
        info!(
            display = display_id,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            stretch = layout.stretch,
            "screen capture started"
        );
        self.running = Some(ScreenRun {
            stream,
            _queue: queue,
            slot,
            sink,
            cfg,
            display_id,
            failure,
        });
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_ref() else {
            bail!("reconfigure before start");
        };
        let failed = run.failure.lock().clone();
        if cfg.display != run.cfg.display || failed.is_some() {
            if let Some(why) = failed {
                info!(%why, "restarting the failed capture");
            }
            let (gate, sink) = (run.slot.gate.clone(), run.sink.clone());
            return self.start(cfg, gate, sink);
        }
        // The runtime passes its current target (a new `start` restarts its controller).
        if cfg.bitrate_bps > 0 {
            self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        }
        let layout = capture_layout(run.display_id, &cfg);
        // Build the new session first so a failure leaves the running one untouched.
        let encoder = self.new_encoder(&cfg, run.slot.gate.clone(), run.sink.clone())?;
        run.stream
            .update_configuration(&stream_configuration(&cfg, layout.stretch)?)
            .context("updating stream configuration")?;
        run.slot.replace_encoder(encoder);
        *run.failure.lock() = None;
        self.publish(layout);
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            stretch = layout.stretch,
            "screen capture reconfigured"
        );
        if let Some(run) = self.running.as_mut() {
            run.cfg = cfg;
        }
        Ok(())
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        let Some(run) = self.running.take() else {
            return;
        };
        if let Err(e) = run.stream.stop_capture() {
            warn!("stopping capture: {e}");
        }
        run.slot.shut_down();
        if let Some(geometry) = &self.geometry {
            geometry.retract(self.id);
        }
        info!("screen capture stopped");
    }

    fn failure(&self) -> Option<String> {
        self.running.as_ref()?.failure.lock().clone()
    }
}

impl Drop for ScreenPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `stretch`: fill the frame even if the display's aspect differs slightly (see
/// [`geometry`]); otherwise letterbox, keeping the aspect.
fn stream_configuration(cfg: &VideoConfig, stretch: bool) -> anyhow::Result<SCStreamConfiguration> {
    Ok(SCStreamConfiguration::new()
        .with_width(cfg.width)
        .with_height(cfg.height)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(!stretch)?
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_color_matrix(color_matrix::ITU_R_709_2)?
        .with_minimum_frame_interval(&CMTime::new(1, cfg.fps as i32))
        .with_queue_depth(SCK_QUEUE_DEPTH)
        .with_shows_cursor(true))
}

/// Capture callback, on the serial capture queue.
fn on_screen_sample(slot: &EncodeSlot, sample: &CMSampleBuffer) {
    let capture_us = now_us();
    if sample.frame_status() != Some(SCFrameStatus::Complete) {
        return; // idle (unchanged), blank, suspended, ...
    }
    let image = sample.image_buffer_ptr_borrowed();
    if image.is_null() {
        return;
    }
    // SAFETY: a +0 CVPixelBufferRef owned by `sample`, which outlives this call. The slot and
    // VideoToolbox retain it for as long as they keep it.
    let image = unsafe { &*image.cast::<CVPixelBuffer>() };
    slot.on_captured(image, capture_us);
}

// ---------------------------------------------------------------------------
// Synthetic

/// Moving NV12 test pattern at the configured size and rate, through the real gate and
/// VideoToolbox encoder. Needs no TCC permission.
pub struct SyntheticPipeline {
    rate_control: RateControl,
    bitrate: BitrateTarget,
    /// Stop rendering new frames after this many, like a static screen (tests).
    freeze_after: Option<u64>,
    running: Option<SyntheticRun>,
}

struct SyntheticRun {
    stop: crossbeam_channel::Sender<()>,
    thread: JoinHandle<()>,
    slot: Arc<EncodeSlot>,
    sink: FrameSink,
    failure: Failure,
}

impl SyntheticPipeline {
    /// Uses [`RateControl::from_env`].
    pub fn new() -> anyhow::Result<Self> {
        Self::with_rate_control(RateControl::from_env())
    }

    pub fn with_rate_control(rate_control: RateControl) -> anyhow::Result<Self> {
        Ok(SyntheticPipeline {
            rate_control,
            bitrate: Arc::new(AtomicU32::new(0)),
            freeze_after: None,
            running: None,
        })
    }

    /// After `frames` frames the pattern stops changing and no new frames are produced, as
    /// ScreenCaptureKit does on a static screen; only keyframe refreshes are encoded.
    #[cfg(test)]
    fn frozen_after(mut self, frames: u64) -> Self {
        self.freeze_after = Some(frames);
        self
    }
}

impl VideoPipeline for SyntheticPipeline {
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<()> {
        self.stop();
        validate(&cfg)?;
        self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        let encoder = H264Encoder::new(
            encoder_config(&cfg, self.rate_control),
            gate.clone(),
            sink.clone(),
            self.bitrate.clone(),
        )?;
        let pattern = TestPattern::new(cfg.width, cfg.height)?;
        let failure = Failure::default();
        let slot = Arc::new(EncodeSlot::new(gate, encoder, failure.clone()));
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let thread_slot = slot.clone();
        let freeze_after = self.freeze_after;
        let thread = std::thread::Builder::new()
            .name("rd-synthetic".into())
            .spawn(move || synthetic_loop(cfg.fps, &thread_slot, pattern, freeze_after, stop_rx))
            .context("spawning synthetic capture thread")?;
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            "synthetic capture started"
        );
        self.running = Some(SyntheticRun {
            stop: stop_tx,
            thread,
            slot,
            sink,
            failure,
        });
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_ref() else {
            bail!("reconfigure before start");
        };
        let (gate, sink) = (run.slot.gate.clone(), run.sink.clone());
        // The runtime passes its current target (a new `start` restarts its controller).
        let bitrate_bps = match cfg.bitrate_bps {
            0 => self.bitrate.load(Ordering::Relaxed),
            bps => bps,
        };
        self.start(VideoConfig { bitrate_bps, ..cfg }, gate, sink)
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        let Some(run) = self.running.take() else {
            return;
        };
        drop(run.stop);
        if run.thread.join().is_err() {
            error!("synthetic capture thread panicked");
        }
        // Normally done by the thread already.
        run.slot.shut_down();
        info!("synthetic capture stopped");
    }

    fn failure(&self) -> Option<String> {
        self.running.as_ref()?.failure.lock().clone()
    }
}

impl Drop for SyntheticPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Produces one frame per tick until `stop` is dropped, then flushes the encoder.
fn synthetic_loop(
    fps: u32,
    slot: &EncodeSlot,
    mut pattern: TestPattern,
    freeze_after: Option<u64>,
    stop: crossbeam_channel::Receiver<()>,
) {
    set_thread_qos_user_interactive();
    let period = Duration::from_secs(1) / fps;
    let mut next = Instant::now();
    let mut frame: u64 = 0;
    // Sleeps until the next tick; ends when `stop` is dropped.
    while let Err(RecvTimeoutError::Timeout) =
        stop.recv_timeout(next.saturating_duration_since(Instant::now()))
    {
        next += period;
        // After a stall (e.g. the machine slept) resume the cadence instead of bursting.
        let now = Instant::now();
        if next < now {
            next = now + period;
        }
        if freeze_after.is_some_and(|n| frame >= n) {
            slot.refresh_tick(now);
            continue;
        }
        // Counts ticks, so frames dropped by the gate show up as jumps in the counter.
        let index = frame;
        frame += 1;
        let capture_us = now_us();
        slot.on_tick(capture_us, || pattern.render(index, capture_us));
    }
    slot.shut_down();
}

fn set_thread_qos_user_interactive() {
    // SAFETY: only changes the calling thread's QoS class.
    let rc = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if rc != 0 {
        warn!(rc, "pthread_set_qos_class_self_np failed");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use rd_agent_core::{Admitted, EncodedFrame};
    use tokio::sync::mpsc;

    use super::*;

    /// Admits everything; forces an IDR on request or after a post-encode drop. Like the
    /// runtime, it wants a refresh while a forced keyframe has not been admitted yet.
    #[derive(Default)]
    struct TestGate {
        next: AtomicU32,
        force: AtomicBool,
        cancelled: AtomicU64,
        post_drops: AtomicU64,
        post_dropped: Mutex<Vec<u32>>,
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
        fn post_encode_drop(&self, seq: u32) {
            self.post_drops.fetch_add(1, Ordering::Relaxed);
            self.post_dropped.lock().push(seq);
            self.force.store(true, Ordering::Relaxed);
        }
        fn refresh_wanted(&self) -> bool {
            self.force.load(Ordering::Relaxed)
        }
    }

    fn video(width: u32, height: u32) -> VideoConfig {
        VideoConfig {
            display: 0,
            width,
            height,
            fps: 60,
            bitrate_bps: 2_000_000,
        }
    }

    fn recv_within(rx: &mut mpsc::Receiver<EncodedFrame>, wait: Duration) -> Option<EncodedFrame> {
        let deadline = Instant::now() + wait;
        loop {
            match rx.try_recv() {
                Ok(frame) => return Some(frame),
                Err(mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return None,
            }
        }
    }

    fn recv(rx: &mut mpsc::Receiver<EncodedFrame>) -> EncodedFrame {
        recv_within(rx, Duration::from_secs(5)).expect("no frame")
    }

    fn reconfigure_and_force_keyframe(rate_control: RateControl) {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let mut pipeline = SyntheticPipeline::with_rate_control(rate_control).unwrap();
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();

        let first = recv(&mut rx);
        let config = first
            .config
            .as_ref()
            .expect("first frame carries the config");
        assert!(first.key);
        assert_eq!((config.width, config.height), (320, 192));
        assert!(config.codec.starts_with("avc1.64"), "{}", config.codec);
        for _ in 0..5 {
            let f = recv(&mut rx);
            assert!(!f.key && f.config.is_none());
        }

        gate.force.store(true, Ordering::Relaxed);
        let keyed = (0..10).map(|_| recv(&mut rx)).find(|f| f.key);
        assert!(
            keyed.is_some_and(|f| f.config.is_none()),
            "forced IDR, same parameter sets"
        );

        pipeline.set_bitrate(1_000_000);
        pipeline.reconfigure(video(640, 384)).unwrap();
        let resized = (0..20)
            .map(|_| recv(&mut rx))
            .find(|f| f.config.is_some())
            .expect("new CodecConfig after reconfigure");
        let config = resized.config.unwrap();
        assert!(resized.key);
        assert_eq!((config.width, config.height), (640, 384));
        assert_eq!(pipeline.failure(), None);

        pipeline.stop();
        while rx.try_recv().is_ok() {}
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert_eq!(gate.post_drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn low_latency_reconfigure_and_keyframes() {
        reconfigure_and_force_keyframe(RateControl::LowLatency);
    }

    #[test]
    fn standard_reconfigure_and_keyframes() {
        reconfigure_and_force_keyframe(RateControl::Standard);
    }

    #[test]
    fn full_sink_drops_after_encode_and_recovers_with_idr() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(1);
        let mut pipeline = SyntheticPipeline::with_rate_control(RateControl::LowLatency).unwrap();
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.post_drops.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "sink never overflowed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let first = recv(&mut rx);
        assert!(first.key && first.config.is_some());
        let recovered = (0..20).map(|_| recv(&mut rx)).find(|f| f.key);
        assert!(recovered.is_some(), "an IDR follows the post-encode drop");
        pipeline.stop();
    }

    /// The viewer never receives a delta whose reference chain contains a frame that was
    /// dropped after encoding: every delivered delta has a delivered keyframe after the last
    /// post-encode drop before it.
    #[test]
    fn no_delta_is_delivered_across_a_post_encode_drop() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(1);
        let mut pipeline = SyntheticPipeline::with_rate_control(RateControl::Standard).unwrap();
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();
        let mut delivered = Vec::new();
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            // A slow consumer: the 1-frame sink overflows every few frames.
            std::thread::sleep(Duration::from_millis(23));
            while let Ok(f) = rx.try_recv() {
                delivered.push((f.seq, f.key));
            }
        }
        pipeline.stop();
        while let Ok(f) = rx.try_recv() {
            delivered.push((f.seq, f.key));
        }
        let dropped = gate.post_dropped.lock().clone();
        assert!(!dropped.is_empty(), "the sink never overflowed");
        assert!(delivered.iter().filter(|(_, key)| *key).count() >= 2);
        let mut last_key = None;
        for &(seq, key) in &delivered {
            if key {
                last_key = Some(seq);
                continue;
            }
            let base = last_key.expect("a delta before any keyframe");
            assert!(
                !dropped.iter().any(|&d| d > base && d < seq),
                "delta {seq} delivered across a drop after keyframe {base}: {dropped:?}"
            );
        }
    }

    /// A static screen produces no frames, so a wanted keyframe is carried by re-submitting
    /// the last frame once the source has been quiet for REFRESH_AFTER; nothing else is
    /// re-submitted.
    #[test]
    fn static_screen_gets_its_keyframe_from_the_last_frame() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let mut pipeline = SyntheticPipeline::with_rate_control(RateControl::Standard)
            .unwrap()
            .frozen_after(5);
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();
        let frames: Vec<_> = (0..5).map(|_| recv(&mut rx)).collect();
        assert!(frames[0].key && frames[0].config.is_some());
        let last_seq = frames[4].seq;
        // Static and nothing wanted: nothing is sent.
        assert!(recv_within(&mut rx, Duration::from_millis(300)).is_none());

        gate.force.store(true, Ordering::Relaxed);
        let asked = Instant::now();
        let refreshed = recv_within(&mut rx, Duration::from_secs(2)).expect("no refresh");
        let took = asked.elapsed();
        assert!(refreshed.key, "the refresh is the forced IDR");
        assert!(
            refreshed.config.is_none(),
            "same session, same parameter sets"
        );
        assert_eq!(refreshed.seq, last_seq + 1, "admitted through the gate");
        assert!(
            took < REFRESH_AFTER + Duration::from_millis(250),
            "{took:?}"
        );
        assert!(!gate.refresh_wanted());
        // Satisfied: no further re-submissions.
        assert!(recv_within(&mut rx, Duration::from_millis(300)).is_none());

        // Again, e.g. a keyframe_req after a decoder error.
        gate.force.store(true, Ordering::Relaxed);
        assert!(recv_within(&mut rx, Duration::from_secs(2)).is_some_and(|f| f.key));
        pipeline.stop();
        assert_eq!(gate.cancelled.load(Ordering::Relaxed), 0);
    }

    /// The ScreenCaptureKit path runs the same check from a timer chain on the capture queue;
    /// the chain ends (and releases the slot) once the slot is shut down.
    #[test]
    fn refresh_timer_on_a_dispatch_queue() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = video(320, 192);
        let bitrate: BitrateTarget = Arc::new(AtomicU32::new(cfg.bitrate_bps));
        let encoder = H264Encoder::new(
            encoder_config(&cfg, RateControl::Standard),
            gate.clone(),
            tx,
            bitrate,
        )
        .unwrap();
        let slot = Arc::new(EncodeSlot::new(gate.clone(), encoder, Failure::default()));
        let mut pattern = TestPattern::new(cfg.width, cfg.height).unwrap();
        let image = pattern.render(0, now_us()).unwrap();
        slot.on_captured(&image, now_us());
        assert!(recv(&mut rx).key);

        let queue = DispatchQueue::new("dev.thijmen.rd.test-refresh", DispatchQoS::Default);
        schedule_refresh(retained_queue(&queue).unwrap(), slot.clone());
        assert!(recv_within(&mut rx, Duration::from_millis(300)).is_none());
        gate.force.store(true, Ordering::Relaxed);
        let refreshed = recv_within(&mut rx, Duration::from_secs(2)).expect("no refresh");
        assert!(refreshed.key && refreshed.seq == 2);
        drop(image);

        slot.shut_down();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&slot) > 1 {
            assert!(Instant::now() < deadline, "the refresh chain did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(queue);
    }

    #[test]
    fn held_frame_marks_its_surface_in_use() {
        #[link(name = "IOSurface", kind = "framework")]
        unsafe extern "C" {
            fn IOSurfaceGetUseCount(surface: NonNull<c_void>) -> i32;
        }
        let mut pattern = TestPattern::new(320, 192).unwrap();
        let image = pattern.render(0, 0).unwrap();
        // SAFETY: valid pixel buffer.
        let surface = unsafe { CVPixelBufferGetIOSurface(&image) }.expect("IOSurface-backed");
        // SAFETY: live surface (kept by `image`).
        let uses = || unsafe { IOSurfaceGetUseCount(surface) };
        // A live CVPixelBuffer already holds one use; the held frame adds its own.
        let before = uses();
        assert!(before >= 1);
        let held = Frame::hold(image.retain());
        assert_eq!(uses(), before + 1);
        drop(held);
        assert_eq!(uses(), before);
    }

    /// A second `start` (or any reconfigure) applies the bitrate it carries.
    #[test]
    fn reconfigure_applies_the_new_bitrate() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(256);
        let mut pipeline = SyntheticPipeline::with_rate_control(RateControl::Standard).unwrap();
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();
        recv(&mut rx);
        let applied = |p: &SyntheticPipeline| {
            let run = p.running.as_ref().unwrap();
            run.slot
                .state
                .lock()
                .encoder
                .as_ref()
                .unwrap()
                .applied_bitrate()
        };
        assert_eq!(applied(&pipeline), 2_000_000);

        pipeline.set_bitrate(20_000_000);
        let restart = VideoConfig {
            bitrate_bps: 4_000_000,
            ..video(320, 192)
        };
        pipeline.reconfigure(restart).unwrap();
        assert_eq!(applied(&pipeline), 4_000_000);
        assert_eq!(pipeline.bitrate.load(Ordering::Relaxed), 4_000_000);

        // 0 keeps the current target.
        pipeline.set_bitrate(3_000_000);
        let keep = VideoConfig {
            bitrate_bps: 0,
            ..video(640, 384)
        };
        pipeline.reconfigure(keep).unwrap();
        assert_eq!(applied(&pipeline), 3_000_000);
        pipeline.stop();
    }

    #[test]
    fn rejects_bad_sizes() {
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let mut pipeline = SyntheticPipeline::new().unwrap();
        assert!(pipeline.start(video(321, 192), gate, tx).is_err());
        assert_eq!(pipeline.failure(), None);
    }

    #[test]
    fn screen_capture_requires_permission() {
        if CGPreflightScreenCaptureAccess() {
            return; // granted: starting would capture the real screen
        }
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let mut pipeline = ScreenPipeline::new().unwrap();
        let err = pipeline.start(video(1280, 828), gate, tx).unwrap_err();
        assert!(err.to_string().contains("Screen Recording"), "{err:#}");
        assert_eq!(pipeline.failure(), None);
    }

    #[test]
    fn layout_follows_the_display_aspect() {
        let main = session::resolve_display(0).unwrap();
        let bounds = CGDisplayBounds(main).size;
        // A frame with the display's own aspect (as the runtime asks for) fills the frame.
        let w = 1280u32;
        let h = ((f64::from(w) * bounds.height / bounds.width / 2.0).round() as u32) * 2;
        let layout = capture_layout(main, &video(w, h));
        assert!(layout.stretch, "{w}x{h} for {bounds:?}");
        assert_eq!((layout.display, layout.width, layout.height), (main, w, h));
        // A clearly different aspect letterboxes.
        assert!(!capture_layout(main, &video(w, w)).stretch);
    }
}
