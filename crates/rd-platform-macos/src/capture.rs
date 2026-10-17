//! Video pipelines: capture (or synthesize) frames, gate them, encode them with VideoToolbox.
//!
//! - [`ScreenPipeline`]: ScreenCaptureKit display capture (NV12 `420v`, scaled on the GPU).
//! - [`SyntheticPipeline`]: a moving NV12 test pattern through the same gate and encoder, for
//!   end-to-end tests without screen-recording permission.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use crossbeam_channel::RecvTimeoutError;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_video::CVPixelBuffer;
use parking_lot::Mutex;
use rd_agent_core::clock::now_us;
use rd_agent_core::{FrameGate, FrameSink, VideoConfig, VideoPipeline};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::color_matrix;
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use tracing::{error, info, warn};

use crate::encoder::{BitrateTarget, EncoderConfig, H264Encoder, RateControl};
use crate::pattern::TestPattern;
use crate::session;

/// Frames ScreenCaptureKit may hold queued for us.
const SCK_QUEUE_DEPTH: u32 = 5;

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

/// Gate + encoder shared with the capture callback.
struct EncodeSlot {
    gate: Arc<dyn FrameGate>,
    /// `None` while stopped. Held for the duration of one admit + encode, which also orders a
    /// replaced encoder's final flush before the first frame of its successor.
    encoder: Mutex<Option<H264Encoder>>,
}

impl EncodeSlot {
    /// Admits and encodes one captured frame.
    fn submit(&self, image: &CVPixelBuffer, capture_us: u64) {
        let mut slot = self.encoder.lock();
        let Some(encoder) = slot.as_mut() else {
            return;
        };
        // Right after a resize ScreenCaptureKit may still deliver a few old-size frames.
        if !encoder.accepts(image) {
            return;
        }
        let Some(admitted) = self.gate.admit(capture_us) else {
            return;
        };
        if let Err(e) = encoder.encode(
            image,
            capture_us,
            admitted.seq,
            capture_us,
            admitted.force_keyframe,
        ) {
            warn!(seq = admitted.seq, "encode failed: {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit

/// ScreenCaptureKit display capture → VideoToolbox.
pub struct ScreenPipeline {
    rate_control: RateControl,
    bitrate: BitrateTarget,
    running: Option<ScreenRun>,
}

struct ScreenRun {
    stream: SCStream,
    /// The serial capture queue; held for as long as the stream delivers on it.
    _queue: DispatchQueue,
    slot: Arc<EncodeSlot>,
    sink: FrameSink,
    cfg: VideoConfig,
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
            running: None,
        })
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

        let encoder = self.new_encoder(&cfg, gate.clone(), sink.clone())?;
        let slot = Arc::new(EncodeSlot {
            gate,
            encoder: Mutex::new(Some(encoder)),
        });
        let delegate =
            StreamCallbacks::new().on_error(|e| error!("ScreenCaptureKit stream stopped: {e}"));
        let mut stream =
            SCStream::new_with_delegate(&filter, &stream_configuration(&cfg)?, delegate)
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
        info!(
            display = display_id,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            "screen capture started"
        );
        self.running = Some(ScreenRun {
            stream,
            _queue: queue,
            slot,
            sink,
            cfg,
        });
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_ref() else {
            bail!("reconfigure before start");
        };
        if cfg.display != run.cfg.display {
            let (gate, sink) = (run.slot.gate.clone(), run.sink.clone());
            return self.start(cfg, gate, sink);
        }
        // Build the new session first so a failure leaves the running one untouched.
        let encoder = self.new_encoder(&cfg, run.slot.gate.clone(), run.sink.clone())?;
        run.stream
            .update_configuration(&stream_configuration(&cfg)?)
            .context("updating stream configuration")?;
        // Dropping the old encoder under the lock flushes its last frames before the new
        // session (whose first frame carries the new CodecConfig) sees any input.
        let mut slot = run.slot.encoder.lock();
        drop(slot.replace(encoder));
        drop(slot);
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
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
        // Flushes and invalidates the session; late capture callbacks find the slot empty.
        drop(run.slot.encoder.lock().take());
        info!("screen capture stopped");
    }
}

impl Drop for ScreenPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stream_configuration(cfg: &VideoConfig) -> anyhow::Result<SCStreamConfiguration> {
    Ok(SCStreamConfiguration::new()
        .with_width(cfg.width)
        .with_height(cfg.height)
        .with_scales_to_fit(true)
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
    // SAFETY: a +0 CVPixelBufferRef owned by `sample`, which outlives this call; VideoToolbox
    // retains it for as long as it needs it.
    let image = unsafe { &*image.cast::<CVPixelBuffer>() };
    slot.submit(image, capture_us);
}

// ---------------------------------------------------------------------------
// Synthetic

/// Moving NV12 test pattern at the configured size and rate, through the real gate and
/// VideoToolbox encoder. Needs no TCC permission.
pub struct SyntheticPipeline {
    rate_control: RateControl,
    bitrate: BitrateTarget,
    running: Option<SyntheticRun>,
}

struct SyntheticRun {
    stop: crossbeam_channel::Sender<()>,
    thread: JoinHandle<()>,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
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
            running: None,
        })
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
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let thread_gate = gate.clone();
        let thread = std::thread::Builder::new()
            .name("rd-synthetic".into())
            .spawn(move || synthetic_loop(cfg.fps, thread_gate, encoder, pattern, stop_rx))
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
            gate,
            sink,
        });
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_ref() else {
            bail!("reconfigure before start");
        };
        let (gate, sink) = (run.gate.clone(), run.sink.clone());
        let bps = self.bitrate.load(Ordering::Relaxed);
        self.start(
            VideoConfig {
                bitrate_bps: bps,
                ..cfg
            },
            gate,
            sink,
        )
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
        info!("synthetic capture stopped");
    }
}

impl Drop for SyntheticPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Produces one frame per tick until `stop` is dropped; the encoder is flushed when it drops
/// at the end of the loop.
fn synthetic_loop(
    fps: u32,
    gate: Arc<dyn FrameGate>,
    mut encoder: H264Encoder,
    mut pattern: TestPattern,
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

        // Counts ticks, so frames dropped by the gate show up as jumps in the counter.
        let index = frame;
        frame += 1;
        let capture_us = now_us();
        let Some(admitted) = gate.admit(capture_us) else {
            continue;
        };
        let image = match pattern.render(index, capture_us) {
            Ok(image) => image,
            Err(e) => {
                warn!("rendering test pattern: {e:#}");
                gate.cancel(admitted.seq);
                continue;
            }
        };
        if let Err(e) = encoder.encode(
            &image,
            capture_us,
            admitted.seq,
            capture_us,
            admitted.force_keyframe,
        ) {
            warn!(seq = admitted.seq, "encode failed: {e:#}");
        }
    }
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
    use std::sync::atomic::AtomicU64;

    use rd_agent_core::{Admitted, EncodedFrame};
    use tokio::sync::mpsc;

    use super::*;

    /// Admits everything; forces an IDR on request or after a post-encode drop.
    #[derive(Default)]
    struct TestGate {
        next: AtomicU32,
        force: std::sync::atomic::AtomicBool,
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

    fn video(width: u32, height: u32) -> VideoConfig {
        VideoConfig {
            display: 0,
            width,
            height,
            fps: 60,
            bitrate_bps: 2_000_000,
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

    #[test]
    fn rejects_bad_sizes() {
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let mut pipeline = SyntheticPipeline::new().unwrap();
        assert!(pipeline.start(video(321, 192), gate, tx).is_err());
    }

    #[test]
    fn screen_capture_requires_permission() {
        if CGPreflightScreenCaptureAccess() {
            return; // granted: starting would capture the real screen
        }
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let err = ScreenPipeline::new()
            .unwrap()
            .start(video(1280, 828), gate, tx)
            .unwrap_err();
        assert!(err.to_string().contains("Screen Recording"), "{err:#}");
    }
}
