//! Video pipelines: capture (or synthesize) frames, gate them, encode them.
//!
//! - [`ScreenPipeline`]: Desktop Duplication of one display; the pointer drawn in; scaled and
//!   converted to NV12 on the GPU; encoded by [`H264Encoder`]. When the display's adapter has
//!   no usable D3D11 device (no graphics driver, a virtual display) it falls back to GDI
//!   capture and CPU conversion ([`crate::gdi`], [`GdiScreen`]).
//! - [`SyntheticPipeline`]: a moving NV12 test pattern through the same gate and encoder, for
//!   end-to-end tests without a desktop to capture.
//!
//! Each pipeline runs on its own thread, which owns every D3D / Media Foundation object of the
//! capture; `reconfigure` and `stop` are commands to it.
//!
//! Desktop Duplication, like ScreenCaptureKit, only delivers frames when something changes. The
//! thread submits at most one frame per `1/fps` (always the newest image, so a burst ends with
//! the screen as it is), and when the runtime wants a keyframe ([`FrameGate::refresh_wanted`])
//! while nothing has changed for [`REFRESH_AFTER`] it re-submits the current image.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use parking_lot::Mutex;
use rd_agent_core::clock::now_us;
use rd_agent_core::pattern::TestPattern;
use rd_agent_core::{FrameGate, FrameSink, VideoConfig, VideoPipeline};
use tracing::{debug, error, info, warn};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIOutput;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

use crate::convert::Converter;
use crate::cursor::Pointer;
use crate::d3d::{self, Gpu};
use crate::display::{self, Monitor};
use crate::duplication::{DuplError, Duplication, Update, desktop_texture};
use crate::encoder::{BitrateTarget, EncoderConfig, EncoderPreference, H264Encoder, Nv12Image};
use crate::geometry::{self, CaptureGeometry, CaptureLayout};
use crate::{gdi, mf};

/// A wanted keyframe is carried by the last frame once the source has been quiet this long.
/// Also the gap between attempts while the gate keeps refusing it.
pub const REFRESH_AFTER: Duration = Duration::from_millis(80);
/// Desktop Duplication is polled this often (see `Screen::poll`).
const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How often a quiet capture thread wakes up (commands, refreshes, retries).
pub const REFRESH_TICK: Duration = Duration::from_millis(50);
/// Opening a duplication again after it was refused (secure desktop) or lost.
const REOPEN_MIN: Duration = Duration::from_millis(250);
const REOPEN_MAX: Duration = Duration::from_secs(2);
/// Device rebuilds in a row (without a working capture in between) before giving up.
const REBUILDS_BEFORE_FAILURE: u32 = 5;

/// Why a pipeline stopped working on its own; see [`VideoPipeline::failure`].
type Failure = Arc<Mutex<Option<String>>>;

fn record_failure(failure: &Mutex<Option<String>>, why: impl FnOnce() -> String) {
    let mut failure = failure.lock();
    if failure.is_none() {
        *failure = Some(why());
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

fn encoder_config(cfg: &VideoConfig, preference: EncoderPreference) -> EncoderConfig {
    EncoderConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        preference,
    }
}

/// 1 ms timer resolution while a capture runs: the frame cadence relies on short waits.
struct TimerResolution;

impl TimerResolution {
    fn new() -> Self {
        // SAFETY: balanced by timeEndPeriod in Drop.
        unsafe { timeBeginPeriod(1) };
        TimerResolution
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        // SAFETY: balances timeBeginPeriod in `new`.
        unsafe { timeEndPeriod(1) };
    }
}

fn raise_thread_priority() {
    // SAFETY: only changes the calling thread's priority.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
        warn!("SetThreadPriority: {e}");
    }
}

enum Command {
    Reconfigure(VideoConfig, Sender<anyhow::Result<()>>),
}

/// A pipeline's worker thread.
struct Worker {
    commands: Sender<Command>,
    thread: JoinHandle<()>,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    cfg: VideoConfig,
    failure: Failure,
}

impl Worker {
    /// Spawns `body`, which sets itself up, reports that through `ready` and then runs until
    /// the command channel closes.
    fn spawn(
        name: &str,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        body: impl FnOnce(Receiver<Command>, Sender<anyhow::Result<()>>, Failure) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let (commands, rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready) = crossbeam_channel::bounded(1);
        let failure = Failure::default();
        let thread_failure = failure.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || body(rx, ready_tx, thread_failure))
            .context("spawning the capture thread")?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Worker {
                commands,
                thread,
                gate,
                sink,
                cfg,
                failure,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                bail!("the capture thread ended during start-up")
            }
        }
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        let (reply, result) = crossbeam_channel::bounded(1);
        self.commands
            .send(Command::Reconfigure(cfg, reply))
            .map_err(|_| anyhow!("the capture thread has ended"))?;
        result
            .recv()
            .map_err(|_| anyhow!("the capture thread has ended"))??;
        self.cfg = cfg;
        Ok(())
    }

    fn stop(self) {
        drop(self.commands);
        if self.thread.join().is_err() {
            error!("capture thread panicked");
        }
    }
}

// ---------------------------------------------------------------------------
// Screen

/// Distinguishes pipelines publishing to the same [`CaptureGeometry`].
static NEXT_PIPELINE_ID: AtomicU64 = AtomicU64::new(1);

/// Desktop Duplication -> pointer -> video processor -> Media Foundation.
pub struct ScreenPipeline {
    preference: EncoderPreference,
    bitrate: BitrateTarget,
    /// Where the captured display sits in the frame, for the input injector.
    geometry: Option<Arc<CaptureGeometry>>,
    id: u64,
    running: Option<Worker>,
}

impl ScreenPipeline {
    pub fn new(preference: EncoderPreference) -> Self {
        ScreenPipeline {
            preference,
            bitrate: Arc::new(AtomicU32::new(0)),
            geometry: None,
            id: NEXT_PIPELINE_ID.fetch_add(1, Ordering::Relaxed),
            running: None,
        }
    }

    /// Publish the capture layout to `geometry` (read by the input injector).
    pub fn with_capture_geometry(mut self, geometry: Arc<CaptureGeometry>) -> Self {
        self.geometry = Some(geometry);
        self
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
        self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        let publish = self.geometry.clone().map(|g| (g, self.id));
        let (thread_gate, thread_sink) = (gate.clone(), sink.clone());
        let (preference, bitrate) = (self.preference, self.bitrate.clone());
        self.running = Some(Worker::spawn(
            "rd-capture",
            cfg,
            gate,
            sink,
            move |commands, ready, failure| {
                let _com = mf::ComThread::init();
                let _timer = TimerResolution::new();
                raise_thread_priority();
                let setup = Source::open(
                    cfg,
                    preference,
                    thread_gate,
                    thread_sink,
                    bitrate,
                    failure,
                    publish,
                );
                let mut source = match setup {
                    Ok(source) => {
                        let _ = ready.send(Ok(()));
                        source
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                source.run(&commands);
                source.shut_down();
            },
        )?);
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_mut() else {
            bail!("reconfigure before start");
        };
        let failed = run.failure.lock().clone();
        if cfg.display != run.cfg.display || failed.is_some() {
            if let Some(why) = failed {
                info!(%why, "restarting the failed capture");
            }
            let (gate, sink) = (run.gate.clone(), run.sink.clone());
            return self.start(cfg, gate, sink);
        }
        // The runtime passes its current target (a new `start` restarts its controller).
        if cfg.bitrate_bps > 0 {
            self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        }
        run.reconfigure(cfg)
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        if let Some(run) = self.running.take() {
            run.stop();
            if let Some(geometry) = &self.geometry {
                geometry.retract(self.id);
            }
            info!("screen capture stopped");
        }
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

/// Environment override for the capture path: `auto` (the default, Desktop Duplication first)
/// or `gdi`, which skips Desktop Duplication to exercise the fallback on a PC that has a
/// graphics driver.
pub const CAPTURE_ENV: &str = "RD_CAPTURE";

/// `RD_CAPTURE=gdi`.
fn gdi_forced() -> bool {
    match std::env::var(CAPTURE_ENV) {
        Err(_) => false,
        Ok(v) if v.eq_ignore_ascii_case("gdi") => true,
        Ok(v) => {
            warn!("{CAPTURE_ENV}: expected auto or gdi, got {v:?}; using auto");
            false
        }
    }
}

/// The capture thread's source of frames: Desktop Duplication (GPU), or GDI when the display's
/// adapter cannot give a D3D11 device with video support (see [`crate::gdi`]).
enum Source {
    Duplication(Box<Screen>),
    Gdi(Box<GdiScreen>),
}

impl Source {
    /// Opens Desktop Duplication of the configured display, or GDI capture when that fails.
    fn open(
        cfg: VideoConfig,
        preference: EncoderPreference,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
        failure: Failure,
        publish: Option<(Arc<CaptureGeometry>, u64)>,
    ) -> anyhow::Result<Self> {
        let reason = if gdi_forced() {
            info!("{CAPTURE_ENV}=gdi: capturing with GDI");
            format!("{CAPTURE_ENV}=gdi")
        } else {
            match Screen::open(
                cfg,
                preference,
                gate.clone(),
                sink.clone(),
                bitrate.clone(),
                failure.clone(),
                publish.clone(),
            ) {
                Ok(screen) => return Ok(Source::Duplication(Box::new(screen))),
                Err(e) => {
                    // A display without a usable graphics driver is the common case (the
                    // Microsoft Basic Display Adapter gives no D3D11 device with video
                    // support): GDI sees it just fine.
                    warn!(
                        display = cfg.display,
                        "desktop duplication is not available ({e:#}); capturing with GDI"
                    );
                    format!("desktop duplication: {e:#}")
                }
            }
        };
        GdiScreen::open(cfg, preference, gate, sink, bitrate, failure, publish)
            .map(|screen| Source::Gdi(Box::new(screen)))
            .with_context(|| format!("GDI capture after {reason}"))
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        match self {
            Source::Duplication(screen) => screen.run(commands),
            Source::Gdi(screen) => screen.run(commands),
        }
    }

    fn shut_down(self) {
        match self {
            Source::Duplication(screen) => screen.shut_down(),
            Source::Gdi(screen) => screen.shut_down(),
        }
    }
}

/// The desktop image (as duplicated), a copy with the pointer drawn in, and a small staging
/// texture for drawing the pointer.
struct DesktopTextures {
    width: u32,
    height: u32,
    desktop: ID3D11Texture2D,
    composite: ID3D11Texture2D,
    pointer_staging: Option<(u32, u32, ID3D11Texture2D)>,
    /// `desktop` holds an image (it is black until the first frame).
    filled: bool,
}

impl DesktopTextures {
    fn new(gpu: &Gpu, width: u32, height: u32) -> anyhow::Result<Self> {
        Ok(DesktopTextures {
            width,
            height,
            desktop: desktop_texture(gpu, width, height)?,
            composite: desktop_texture(gpu, width, height)?,
            pointer_staging: None,
            filled: false,
        })
    }

    /// The texture to scale: `desktop`, or `composite` with the pointer drawn in.
    fn compose(&mut self, gpu: &Gpu, pointer: &Pointer) -> anyhow::Result<&ID3D11Texture2D> {
        let (Some((x, y, w, h)), Some(shape)) = (
            pointer.bounds(self.width, self.height),
            pointer.shape.as_ref(),
        ) else {
            return Ok(&self.desktop);
        };
        let staging = match &self.pointer_staging {
            Some((sw, sh, t)) if *sw >= w && *sh >= h => t.clone(),
            _ => {
                let (sw, sh) = (w.max(64), h.max(64));
                let t = staging_bgra(gpu, sw, sh)?;
                self.pointer_staging = Some((sw, sh, t.clone()));
                t
            }
        };
        let region = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + w,
            bottom: y + h,
            back: 1,
        };
        let local = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w,
            bottom: h,
            back: 1,
        };
        // SAFETY: BGRA textures on `gpu`; the boxes lie within them; the mapping is accessed
        // within `h` rows of `RowPitch` bytes, `w` pixels each.
        unsafe {
            let ctx = &gpu.context;
            ctx.CopyResource(&self.composite, &self.desktop);
            ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.desktop, 0, Some(&region));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))
                .context("mapping the pointer staging texture")?;
            let pitch = mapped.RowPitch as usize;
            let pixels = std::slice::from_raw_parts_mut(
                mapped.pData.cast::<u8>(),
                pitch * (h as usize - 1) + w as usize * 4,
            );
            shape.draw(
                pixels,
                pitch,
                w,
                h,
                pointer.x - x as i32,
                pointer.y - y as i32,
            );
            ctx.Unmap(&staging, 0);
            ctx.CopySubresourceRegion(&self.composite, 0, x, y, 0, &staging, 0, Some(&local));
        }
        Ok(&self.composite)
    }
}

fn staging_bgra(gpu: &Gpu, width: u32, height: u32) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid description; out pointer to a local.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("creating the pointer staging texture")?;
    texture.context("CreateTexture2D returned no texture")
}

/// The capture thread's state.
struct Screen {
    cfg: VideoConfig,
    monitor: Monitor,
    preference: EncoderPreference,
    gpu: Gpu,
    output: IDXGIOutput,
    dupl: Option<Duplication>,
    textures: DesktopTextures,
    converter: Converter,
    encoder: H264Encoder,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    bitrate: BitrateTarget,
    failure: Failure,
    publish: Option<(Arc<CaptureGeometry>, u64)>,
    /// The image changed since the last submit.
    dirty: bool,
    /// Agent clock when the current image was captured.
    captured_us: u64,
    last_submit: Option<Instant>,
    /// When the image last changed or was last (re-)submitted.
    last_activity: Instant,
    reopen_at: Instant,
    reopen_backoff: Duration,
    /// The duplication is being refused (secure desktop); logged once per period.
    denied: bool,
    rebuilds: u32,
}

impl Screen {
    fn open(
        cfg: VideoConfig,
        preference: EncoderPreference,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
        failure: Failure,
        publish: Option<(Arc<CaptureGeometry>, u64)>,
    ) -> anyhow::Result<Self> {
        mf::ensure_started()?;
        let monitor = display::resolve(cfg.display)
            .with_context(|| format!("display {} is not attached", cfg.display))?;
        let (adapter, output, _) = d3d::find_output(&monitor.device)?;
        let gpu = d3d::create_device(Some(&adapter))?;
        // Refused while the secure desktop is up: start anyway and keep trying.
        let (dupl, denied) = match Duplication::open(&gpu, &output) {
            Ok(d) => (Some(d), false),
            Err(DuplError::Denied(e)) => {
                info!("the desktop cannot be captured right now ({e}); retrying");
                (None, true)
            }
            Err(e) => bail!("cannot capture {}: {e}", monitor.device),
        };
        let (width, height) = dupl
            .as_ref()
            .map_or((monitor.rect.w, monitor.rect.h), |d| (d.width, d.height));
        if dupl.as_ref().is_some_and(Duplication::rotated) {
            warn!(
                display = cfg.display,
                "the display is rotated; its image is captured in landscape (scan-out) orientation"
            );
        }
        let textures = DesktopTextures::new(&gpu, width, height)?;
        let layout = layout_for(&cfg, width, height);
        let converter = Converter::new(
            &gpu,
            (width, height),
            (cfg.width, cfg.height),
            geometry::content_pixels(&layout, width, height),
            cfg.fps,
        )?;
        let encoder = H264Encoder::new(
            encoder_config(&cfg, preference),
            Some(gpu.clone()),
            gate.clone(),
            sink.clone(),
            bitrate.clone(),
        )?;
        info!(
            display = cfg.display,
            device = %monitor.device,
            adapter = %gpu.name,
            desktop = format_args!("{width}x{height}"),
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            stretch = layout.stretch,
            encoder = %encoder.name(),
            "screen capture started"
        );
        let screen = Screen {
            cfg,
            monitor,
            preference,
            gpu,
            output,
            dupl,
            textures,
            converter,
            encoder,
            gate,
            sink,
            bitrate,
            failure,
            publish,
            dirty: false,
            captured_us: now_us(),
            last_submit: None,
            last_activity: Instant::now(),
            reopen_at: Instant::now(),
            reopen_backoff: REOPEN_MIN,
            denied,
            rebuilds: 0,
        };
        screen.publish_layout(&layout);
        Ok(screen)
    }

    fn publish_layout(&self, layout: &CaptureLayout) {
        if let Some((geometry, id)) = &self.publish {
            geometry.publish(*id, *layout);
        }
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        let mut interval = Duration::from_secs(1) / self.cfg.fps.max(1);
        loop {
            match commands.try_recv() {
                Ok(Command::Reconfigure(cfg, reply)) => {
                    let result = self.reconfigure(cfg);
                    if result.is_ok() {
                        interval = Duration::from_secs(1) / cfg.fps.max(1);
                    }
                    let _ = reply.send(result);
                }
                Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            let now = Instant::now();
            self.encoder.maintain(now);
            if let Some(fatal) = self.encoder.fatal() {
                record_failure(&self.failure, || fatal.to_owned());
            }
            // Wait for the next update: until the frame interval allows the pending image
            // out, or a refresh tick.
            let wait = if self.dirty {
                self.last_submit
                    .map_or(Duration::ZERO, |t| interval.saturating_sub(now - t))
            } else {
                REFRESH_TICK
            };
            if self.dupl.is_none() {
                if now >= self.reopen_at {
                    self.reopen();
                }
                if self.dupl.is_none() {
                    // Commands still get through while waiting.
                    match commands.recv_timeout(wait.min(REFRESH_TICK)) {
                        Ok(Command::Reconfigure(cfg, reply)) => {
                            let result = self.reconfigure(cfg);
                            if result.is_ok() {
                                interval = Duration::from_secs(1) / cfg.fps.max(1);
                            }
                            let _ = reply.send(result);
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
            } else {
                self.poll(wait);
            }
            self.maybe_submit(interval);
        }
    }

    /// Waits up to `wait` for the next desktop update. The duplication is polled without a
    /// timeout, sleeping [`POLL_INTERVAL`] in between: while `AcquireNextFrame` waits it holds
    /// the device's multithread lock, which stalls the encoder working on the same device
    /// (measured: encode p50 7.8 ms with 1 ms waits, 13-32 ms with 50 ms waits).
    fn poll(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        loop {
            let Some(dupl) = self.dupl.as_mut() else {
                return;
            };
            match dupl.next(0, &self.textures.desktop, &self.gpu) {
                Ok(Update::Timeout) => {}
                Ok(Update::Frame { desktop, pointer }) => {
                    if desktop {
                        self.textures.filled = true;
                    }
                    if desktop || pointer {
                        self.dirty = true;
                        self.captured_us = now_us();
                        self.last_activity = Instant::now();
                    }
                    self.rebuilds = 0;
                    self.reopen_backoff = REOPEN_MIN;
                    return;
                }
                Err(e) => return self.lost(e),
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    fn lost(&mut self, e: DuplError) {
        self.dupl = None;
        match e {
            DuplError::Denied(e) => {
                if !self.denied {
                    info!("the desktop cannot be captured right now ({e}); retrying");
                    self.denied = true;
                }
                self.reopen_at = Instant::now() + REOPEN_MIN;
            }
            DuplError::Lost(e) => {
                debug!("desktop duplication ended ({e}); reopening");
                self.reopen_at = Instant::now();
            }
            DuplError::DeviceLost(e) | DuplError::Unsupported(e) => {
                warn!("screen capture lost its device ({e}); rebuilding");
                self.rebuild();
            }
        }
    }

    /// Opens a new duplication, following a changed display mode.
    fn reopen(&mut self) {
        match Duplication::open(&self.gpu, &self.output) {
            Ok(dupl) => {
                if self.denied {
                    info!("the desktop can be captured again");
                    self.denied = false;
                }
                let size = (dupl.width, dupl.height);
                // The pointer state starts over; the next frame re-reports it.
                self.dupl = Some(dupl);
                if size != (self.textures.width, self.textures.height)
                    && let Err(e) = self.resize_desktop(size.0, size.1)
                {
                    warn!("following the display mode change: {e:#}");
                    self.dupl = None;
                    self.reopen_at = Instant::now() + self.reopen_backoff;
                }
                self.reopen_backoff = REOPEN_MIN;
            }
            Err(DuplError::Denied(_)) => {
                self.denied = true;
                self.reopen_at = Instant::now() + REOPEN_MIN;
            }
            Err(DuplError::Lost(e)) => {
                debug!("reopening desktop duplication: {e}");
                self.reopen_at = Instant::now() + self.reopen_backoff;
                self.reopen_backoff = (self.reopen_backoff * 2).min(REOPEN_MAX);
            }
            Err(DuplError::DeviceLost(e) | DuplError::Unsupported(e)) => {
                warn!("reopening desktop duplication: {e}; rebuilding the device");
                self.rebuild();
            }
        }
    }

    /// The display mode changed: new desktop textures and scaler, same frame and encoder.
    fn resize_desktop(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        let layout = layout_for(&self.cfg, width, height);
        let converter = Converter::new(
            &self.gpu,
            (width, height),
            (self.cfg.width, self.cfg.height),
            geometry::content_pixels(&layout, width, height),
            self.cfg.fps,
        )?;
        self.textures = DesktopTextures::new(&self.gpu, width, height)?;
        self.converter = converter;
        self.publish_layout(&layout);
        if let Some(monitor) = display::resolve(self.cfg.display) {
            self.monitor = monitor;
        }
        info!(
            desktop = format_args!("{width}x{height}"),
            stretch = layout.stretch,
            "display mode changed"
        );
        Ok(())
    }

    /// The device is gone (or the display moved to another adapter): rebuild everything.
    fn rebuild(&mut self) {
        self.rebuilds += 1;
        if self.rebuilds > REBUILDS_BEFORE_FAILURE {
            record_failure(&self.failure, || {
                format!(
                    "screen capture of display {} keeps failing",
                    self.cfg.display
                )
            });
            self.reopen_at = Instant::now() + REOPEN_MAX;
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            let monitor = display::resolve(self.cfg.display)
                .with_context(|| format!("display {} is no longer attached", self.cfg.display))?;
            let (adapter, output, _) = d3d::find_output(&monitor.device)?;
            let gpu = d3d::create_device(Some(&adapter))?;
            let (w, h) = (monitor.rect.w, monitor.rect.h);
            let layout = layout_for(&self.cfg, w, h);
            let converter = Converter::new(
                &gpu,
                (w, h),
                (self.cfg.width, self.cfg.height),
                geometry::content_pixels(&layout, w, h),
                self.cfg.fps,
            )?;
            let textures = DesktopTextures::new(&gpu, w, h)?;
            let encoder = H264Encoder::new(
                encoder_config(&self.cfg, self.preference),
                Some(gpu.clone()),
                self.gate.clone(),
                self.sink.clone(),
                self.bitrate.clone(),
            )?;
            self.monitor = monitor;
            self.output = output;
            self.gpu = gpu;
            self.converter = converter;
            self.textures = textures;
            self.encoder = encoder;
            self.publish_layout(&layout);
            Ok(())
        })();
        match result {
            Ok(()) => info!(adapter = %self.gpu.name, "screen capture rebuilt"),
            Err(e) => {
                warn!("rebuilding the screen capture: {e:#}");
                if e.to_string().contains("no longer attached") {
                    record_failure(&self.failure, || format!("{e:#}"));
                }
            }
        }
        self.reopen_at = Instant::now() + self.reopen_backoff;
        self.reopen_backoff = (self.reopen_backoff * 2).min(REOPEN_MAX);
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        let (w, h) = (self.textures.width, self.textures.height);
        let layout = layout_for(&cfg, w, h);
        // Build the new parts first so a failure leaves the running ones untouched.
        let converter = Converter::new(
            &self.gpu,
            (w, h),
            (cfg.width, cfg.height),
            geometry::content_pixels(&layout, w, h),
            cfg.fps,
        )?;
        let encoder = H264Encoder::new(
            encoder_config(&cfg, self.preference),
            Some(self.gpu.clone()),
            self.gate.clone(),
            self.sink.clone(),
            self.bitrate.clone(),
        )?;
        // The old encoder drains before its successor sees any input.
        self.encoder = encoder;
        self.converter = converter;
        self.cfg = cfg;
        *self.failure.lock() = None;
        self.publish_layout(&layout);
        // The new encoder starts with the current image.
        self.dirty = self.textures.filled;
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            stretch = layout.stretch,
            "screen capture reconfigured"
        );
        Ok(())
    }

    /// Submits the current image if it changed and the frame interval allows, or re-submits
    /// it when a keyframe is wanted and the screen has been quiet.
    fn maybe_submit(&mut self, interval: Duration) {
        let now = Instant::now();
        if self.dirty {
            // A little slack so a 60 Hz desktop is not halved by timer jitter.
            let due = self
                .last_submit
                .is_none_or(|t| now - t + Duration::from_millis(1) >= interval);
            if due {
                self.dirty = false;
                self.last_submit = Some(now);
                self.submit(self.captured_us);
            }
            return;
        }
        let quiet = now.saturating_duration_since(self.last_activity) >= REFRESH_AFTER;
        if quiet
            && self.textures.filled
            && (self.gate.refresh_wanted() || self.encoder.keyframe_pending())
        {
            debug!("quiet screen: re-submitting the current image to carry a keyframe");
            self.last_activity = now;
            self.last_submit = Some(now);
            // Stamped now: that is when it is shown.
            self.submit(now_us());
        }
    }

    fn submit(&mut self, capture_us: u64) {
        let Some(admitted) = self.gate.admit(capture_us) else {
            return;
        };
        let Some(surface) = self.encoder.acquire_surface() else {
            debug!(seq = admitted.seq, "no free encoder surface; frame dropped");
            self.gate.cancel(admitted.seq);
            return;
        };
        let pointer = self
            .dupl
            .as_ref()
            .map(|d| d.pointer.clone())
            .unwrap_or_default();
        let source = match self.textures.compose(&self.gpu, &pointer) {
            Ok(texture) => texture.clone(),
            Err(e) => {
                debug!("drawing the pointer: {e:#}");
                self.textures.desktop.clone()
            }
        };
        if let Err(e) = self.converter.convert(&source, surface.texture()) {
            warn!(seq = admitted.seq, "scaling the desktop: {e:#}");
            self.encoder.release_surface(surface);
            self.gate.cancel(admitted.seq);
            return;
        }
        if let Err(e) =
            self.encoder
                .encode_surface(surface, capture_us, admitted.seq, admitted.force_keyframe)
        {
            debug!(seq = admitted.seq, "encode failed: {e:#}");
        }
    }

    fn shut_down(self) {
        let Screen { encoder, dupl, .. } = self;
        // The encoder drains (delivering what it still holds) before the capture goes.
        drop(encoder);
        drop(dupl);
    }
}

/// How a `width x height` desktop is laid out in a `cfg` frame (see [`geometry`]).
fn layout_for(cfg: &VideoConfig, width: u32, height: u32) -> CaptureLayout {
    CaptureLayout {
        display: cfg.display,
        width: cfg.width,
        height: cfg.height,
        stretch: geometry::stretch_to_fill(
            f64::from(width),
            f64::from(height),
            cfg.width,
            cfg.height,
        ),
    }
}

// ---------------------------------------------------------------------------
// GDI (no D3D11 on the display's adapter)

/// How long between checks that the display is still there and the same size.
const DISPLAY_CHECK: Duration = Duration::from_millis(500);
/// A copy that fails this long is reported (a secure desktop or a mode switch passes).
const CAPTURE_GRACE: Duration = Duration::from_secs(5);

/// A display captured with GDI, screenshot-style, and encoded from system memory: the fallback
/// when Desktop Duplication is unavailable (see [`Source::open`]).
///
/// It cannot be told when something changed, so it captures every tick and compares the image
/// with the previous one; only changes reach the encoder, plus the refreshes a static image
/// needs for keyframes, as the synthetic source and `Screen` do.
struct GdiScreen {
    cfg: VideoConfig,
    preference: EncoderPreference,
    /// The default adapter, for a hardware encoder to upload to; `None` for software encoding
    /// (also when the machine has no D3D11 at all).
    gpu: Option<Gpu>,
    capture: gdi::Capture,
    image: Nv12Image,
    encoder: H264Encoder,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    bitrate: BitrateTarget,
    failure: Failure,
    publish: Option<(Arc<CaptureGeometry>, u64)>,
    /// When the display was last checked for a mode change.
    checked: Instant,
    last_submit: Instant,
    /// Since when the desktop cannot be copied (see [`CAPTURE_GRACE`]).
    failed_since: Option<Instant>,
}

impl GdiScreen {
    fn open(
        cfg: VideoConfig,
        preference: EncoderPreference,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
        failure: Failure,
        publish: Option<(Arc<CaptureGeometry>, u64)>,
    ) -> anyhow::Result<Self> {
        mf::ensure_started()?;
        let monitor = display::resolve(cfg.display)
            .with_context(|| format!("display {} is not attached", cfg.display))?;
        let capture = open_capture(&cfg, &monitor)?;
        // The default adapter's hardware encoder if there is one, else software: frames are
        // uploaded from system memory when the encoder is a GPU's own MFT. A WARP device would
        // bring nothing here (no hardware encoder on it).
        let gpu = match preference {
            EncoderPreference::Software => None,
            _ => d3d::create_device(None)
                .inspect_err(|e| debug!("no D3D11 device for the GDI capture: {e:#}"))
                .ok()
                .filter(|gpu| !gpu.software),
        };
        let encoder = H264Encoder::new(
            encoder_config(&cfg, preference),
            gpu.clone(),
            gate.clone(),
            sink.clone(),
            bitrate.clone(),
        )?;
        info!(
            display = cfg.display,
            device = %monitor.device,
            desktop = format_args!("{}x{}", monitor.rect.w, monitor.rect.h),
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps.min(gdi::MAX_FPS),
            encoder = %encoder.name(),
            "GDI screen capture started"
        );
        let screen = GdiScreen {
            capture,
            image: Nv12Image::new(cfg.width, cfg.height),
            cfg,
            preference,
            gpu,
            encoder,
            gate,
            sink,
            bitrate,
            failure,
            publish,
            checked: Instant::now(),
            last_submit: Instant::now(),
            failed_since: None,
        };
        screen.publish_layout();
        Ok(screen)
    }

    /// The frame rate is capped: this path copies the desktop through the CPU.
    fn period(&self) -> Duration {
        Duration::from_secs(1) / self.cfg.fps.clamp(1, gdi::MAX_FPS)
    }

    fn publish_layout(&self) {
        if let Some((geometry, id)) = &self.publish {
            let monitor = self.capture.monitor();
            geometry.publish(*id, layout_for(&self.cfg, monitor.rect.w, monitor.rect.h));
        }
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        let encoder = H264Encoder::new(
            encoder_config(&cfg, self.preference),
            self.gpu.clone(),
            self.gate.clone(),
            self.sink.clone(),
            self.bitrate.clone(),
        )?;
        // New frame size: a new capture surface and image, beside the running ones until both
        // exist.
        let monitor = self.capture.monitor().clone();
        let capture = open_capture(&cfg, &monitor)?;
        self.capture = capture;
        self.image = Nv12Image::new(cfg.width, cfg.height);
        self.encoder = encoder;
        self.cfg = cfg;
        self.checked = Instant::now();
        self.publish_layout();
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps.min(gdi::MAX_FPS),
            "GDI screen capture reconfigured"
        );
        Ok(())
    }

    /// Follows a display that was detached, added back, or resized (a mode change).
    fn follow_display(&mut self) -> anyhow::Result<()> {
        let monitor = display::resolve(self.cfg.display)
            .with_context(|| format!("display {} is no longer attached", self.cfg.display))?;
        if monitor == *self.capture.monitor() {
            return Ok(());
        }
        let content = content_for(&self.cfg, &monitor.rect);
        let (width, height) = (monitor.rect.w, monitor.rect.h);
        self.capture
            .resize(monitor, self.cfg.width, self.cfg.height, content)?;
        self.publish_layout();
        info!(
            display = self.cfg.display,
            desktop = format_args!("{width}x{height}"),
            "display mode changed"
        );
        Ok(())
    }

    /// Captures at most `1/fps`; only a changed image is encoded, unless a keyframe is wanted.
    fn run(&mut self, commands: &Receiver<Command>) {
        let mut period = self.period();
        let mut next = Instant::now();
        loop {
            match commands.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Command::Reconfigure(cfg, reply)) => {
                    let result = self.reconfigure(cfg);
                    if result.is_ok() {
                        period = self.period();
                    }
                    let _ = reply.send(result);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            next += period;
            // After a stall (e.g. the machine slept) resume the cadence instead of bursting.
            let now = Instant::now();
            if next < now {
                next = now + period;
            }
            self.encoder.maintain(now);
            if let Some(fatal) = self.encoder.fatal() {
                record_failure(&self.failure, || fatal.to_owned());
            }
            if now.saturating_duration_since(self.checked) >= DISPLAY_CHECK {
                self.checked = now;
                if let Err(e) = self.follow_display() {
                    record_failure(&self.failure, || format!("{e:#}"));
                    return;
                }
            }
            let capture_us = now_us();
            match self.capture.capture(&mut self.image) {
                Ok(true) => {
                    self.failed_since = None;
                    self.submit(capture_us);
                }
                Ok(false) => {
                    self.failed_since = None;
                    let quiet = now.saturating_duration_since(self.last_submit) >= REFRESH_AFTER;
                    if quiet && (self.gate.refresh_wanted() || self.encoder.keyframe_pending()) {
                        debug!("quiet screen: re-submitting the current image to carry a keyframe");
                        self.submit(now_us());
                    }
                }
                // The desktop can refuse a copy for a while (a secure desktop, a mode switch):
                // keep the last picture and try again, as Desktop Duplication does. Only a
                // failure that lasts is reported.
                Err(e) => match self.failed_since {
                    None => {
                        debug!(
                            display = self.cfg.display,
                            "the desktop cannot be copied ({e:#}); keeping the last picture"
                        );
                        self.failed_since = Some(now);
                    }
                    Some(since) if now.saturating_duration_since(since) >= CAPTURE_GRACE => {
                        record_failure(&self.failure, || {
                            format!("capturing display {}: {e:#}", self.cfg.display)
                        });
                        return;
                    }
                    Some(_) => {}
                },
            }
        }
    }

    fn submit(&mut self, capture_us: u64) {
        self.last_submit = Instant::now();
        let Some(admitted) = self.gate.admit(capture_us) else {
            return;
        };
        if let Err(e) = self.encoder.encode_image(
            &self.image,
            capture_us,
            admitted.seq,
            admitted.force_keyframe,
        ) {
            debug!(seq = admitted.seq, "encode failed: {e:#}");
        }
    }

    fn shut_down(self) {
        // The encoder drains (delivering what it still holds).
        drop(self.encoder);
    }
}

/// A GDI capture of `cfg.display`'s monitor, laid out for `cfg`'s frame.
fn open_capture(cfg: &VideoConfig, monitor: &Monitor) -> anyhow::Result<gdi::Capture> {
    let content = content_for(cfg, &monitor.rect);
    gdi::Capture::new(monitor.clone(), cfg.width, cfg.height, content)
}

/// Where a display of `display` sits in `cfg`'s frame, pixels.
fn content_for(cfg: &VideoConfig, display: &display::Rect) -> (i32, i32, i32, i32) {
    geometry::content_pixels(&layout_for(cfg, display.w, display.h), display.w, display.h)
}

// ---------------------------------------------------------------------------
// Synthetic

/// Moving NV12 test pattern at the configured size and rate, through the real gate and
/// encoder. Needs no desktop.
pub struct SyntheticPipeline {
    preference: EncoderPreference,
    bitrate: BitrateTarget,
    /// Stop rendering new frames after this many, like a static screen (tests).
    freeze_after: Option<u64>,
    running: Option<Worker>,
}

impl SyntheticPipeline {
    pub fn new(preference: EncoderPreference) -> Self {
        SyntheticPipeline {
            preference,
            bitrate: Arc::new(AtomicU32::new(0)),
            freeze_after: None,
            running: None,
        }
    }

    /// After `frames` frames the pattern stops changing and no new frames are produced, as
    /// on a static screen; only keyframe refreshes are encoded.
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
        let (thread_gate, thread_sink) = (gate.clone(), sink.clone());
        let (preference, bitrate, freeze_after) =
            (self.preference, self.bitrate.clone(), self.freeze_after);
        self.running = Some(Worker::spawn(
            "rd-synthetic",
            cfg,
            gate,
            sink,
            move |commands, ready, failure| {
                let _com = mf::ComThread::init();
                let _timer = TimerResolution::new();
                let setup = Synthetic::open(cfg, preference, thread_gate, thread_sink, bitrate);
                let mut synthetic = match setup {
                    Ok(s) => {
                        let _ = ready.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                synthetic.freeze_after = freeze_after;
                synthetic.run(&commands, &failure);
            },
        )?);
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        validate(&cfg)?;
        let Some(run) = self.running.as_mut() else {
            bail!("reconfigure before start");
        };
        if cfg.bitrate_bps > 0 {
            self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        }
        run.reconfigure(cfg)
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        if let Some(run) = self.running.take() {
            run.stop();
            info!("synthetic capture stopped");
        }
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

struct Synthetic {
    cfg: VideoConfig,
    preference: EncoderPreference,
    gpu: Option<Gpu>,
    encoder: H264Encoder,
    pattern: TestPattern,
    image: Nv12Image,
    gate: Arc<dyn FrameGate>,
    sink: FrameSink,
    bitrate: BitrateTarget,
    freeze_after: Option<u64>,
}

impl Synthetic {
    fn open(
        cfg: VideoConfig,
        preference: EncoderPreference,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
        bitrate: BitrateTarget,
    ) -> anyhow::Result<Self> {
        mf::ensure_started()?;
        // The default adapter's hardware encoder if there is one, else software.
        let gpu = match preference {
            EncoderPreference::Software => None,
            _ => d3d::create_device(None)
                .inspect_err(|e| debug!("no D3D11 device for the synthetic source: {e:#}"))
                .ok(),
        };
        let encoder = H264Encoder::new(
            encoder_config(&cfg, preference),
            gpu.clone(),
            gate.clone(),
            sink.clone(),
            bitrate.clone(),
        )?;
        info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            encoder = %encoder.name(),
            "synthetic capture started"
        );
        Ok(Synthetic {
            pattern: TestPattern::new(cfg.width, cfg.height)?,
            image: Nv12Image::new(cfg.width, cfg.height),
            cfg,
            preference,
            gpu,
            encoder,
            gate,
            sink,
            bitrate,
            freeze_after: None,
        })
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        let encoder = H264Encoder::new(
            encoder_config(&cfg, self.preference),
            self.gpu.clone(),
            self.gate.clone(),
            self.sink.clone(),
            self.bitrate.clone(),
        )?;
        self.pattern = TestPattern::new(cfg.width, cfg.height)?;
        self.image = Nv12Image::new(cfg.width, cfg.height);
        self.encoder = encoder;
        self.cfg = cfg;
        Ok(())
    }

    /// Produces one frame per tick until the command channel closes.
    fn run(&mut self, commands: &Receiver<Command>, failure: &Failure) {
        let mut period = Duration::from_secs(1) / self.cfg.fps;
        let mut next = Instant::now();
        let mut frame: u64 = 0;
        let mut last_submit = Instant::now();
        loop {
            match commands.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Command::Reconfigure(cfg, reply)) => {
                    let result = self.reconfigure(cfg);
                    if result.is_ok() {
                        period = Duration::from_secs(1) / cfg.fps;
                    }
                    let _ = reply.send(result);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            next += period;
            // After a stall (e.g. the machine slept) resume the cadence instead of bursting.
            let now = Instant::now();
            if next < now {
                next = now + period;
            }
            self.encoder.maintain(now);
            if let Some(fatal) = self.encoder.fatal() {
                record_failure(failure, || fatal.to_owned());
            }
            if self.freeze_after.is_some_and(|n| frame >= n) {
                let quiet = now.saturating_duration_since(last_submit) >= REFRESH_AFTER;
                if quiet && (self.gate.refresh_wanted() || self.encoder.keyframe_pending()) {
                    last_submit = now;
                    self.encode(now_us());
                }
                continue;
            }
            // Counts ticks, so frames dropped by the gate show up as jumps in the counter.
            let index = frame;
            frame += 1;
            last_submit = now;
            let capture_us = now_us();
            let Some(admitted) = self.gate.admit(capture_us) else {
                continue;
            };
            self.pattern
                .draw(&mut self.image.planes(), index, capture_us);
            if let Err(e) = self.encoder.encode_image(
                &self.image,
                capture_us,
                admitted.seq,
                admitted.force_keyframe,
            ) {
                debug!(seq = admitted.seq, "encode failed: {e:#}");
            }
        }
    }

    /// Re-encodes the last image (a keyframe refresh).
    fn encode(&mut self, capture_us: u64) {
        let Some(admitted) = self.gate.admit(capture_us) else {
            return;
        };
        if let Err(e) = self.encoder.encode_image(
            &self.image,
            capture_us,
            admitted.seq,
            admitted.force_keyframe,
        ) {
            debug!(seq = admitted.seq, "encode failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

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

    fn reconfigure_and_force_keyframe(preference: EncoderPreference) {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let mut pipeline = SyntheticPipeline::new(preference);
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();

        let first = recv(&mut rx);
        let config = first
            .config
            .as_ref()
            .expect("the first frame carries the config");
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
    fn software_reconfigure_and_keyframes() {
        reconfigure_and_force_keyframe(EncoderPreference::Software);
    }

    #[test]
    fn default_encoder_reconfigure_and_keyframes() {
        reconfigure_and_force_keyframe(EncoderPreference::Auto);
    }

    /// The viewer never receives a delta whose reference chain contains a frame that was
    /// dropped after encoding.
    #[test]
    fn no_delta_is_delivered_across_a_post_encode_drop() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(1);
        let mut pipeline = SyntheticPipeline::new(EncoderPreference::Auto);
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
    /// the last frame once the source has been quiet for REFRESH_AFTER.
    #[test]
    fn static_screen_gets_its_keyframe_from_the_last_frame() {
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let mut pipeline = SyntheticPipeline::new(EncoderPreference::Auto).frozen_after(5);
        pipeline.start(video(320, 192), gate.clone(), tx).unwrap();
        let frames: Vec<_> = (0..5).map(|_| recv(&mut rx)).collect();
        assert!(frames[0].key && frames[0].config.is_some());
        let last_seq = frames[4].seq;
        assert!(recv_within(&mut rx, Duration::from_millis(300)).is_none());

        gate.force.store(true, Ordering::Relaxed);
        let asked = Instant::now();
        let refreshed = recv_within(&mut rx, Duration::from_secs(2)).expect("no refresh");
        let took = asked.elapsed();
        assert!(refreshed.key, "the refresh is the forced IDR");
        assert!(refreshed.config.is_none(), "same parameter sets");
        assert_eq!(refreshed.seq, last_seq + 1, "admitted through the gate");
        assert!(
            took < REFRESH_AFTER + Duration::from_millis(250),
            "{took:?}"
        );
        assert!(!gate.refresh_wanted());
        assert!(recv_within(&mut rx, Duration::from_millis(300)).is_none());
        pipeline.stop();
        assert_eq!(gate.cancelled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rejects_bad_sizes() {
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let mut pipeline = SyntheticPipeline::new(EncoderPreference::Auto);
        assert!(pipeline.start(video(321, 192), gate, tx).is_err());
        assert_eq!(pipeline.failure(), None);
    }

    /// A display that is not attached fails both the Duplication and the GDI path, and the
    /// error names both (the GDI one is the fallback for displays without a D3D11 device).
    #[test]
    fn missing_display_reports_both_capture_paths() {
        let gate = Arc::new(TestGate::default());
        let (tx, _rx) = mpsc::channel(8);
        let mut pipeline = ScreenPipeline::new(EncoderPreference::Auto);
        let mut cfg = video(320, 192);
        cfg.display = u32::MAX - 1;
        let err = pipeline.start(cfg, gate, tx).expect_err("no such display");
        let msg = format!("{err:#}");
        assert!(msg.contains("is not attached"), "{msg}");
        assert!(msg.contains("GDI capture after"), "{msg}");
        assert_eq!(pipeline.failure(), None);
    }

    /// Real capture of the primary display (Desktop Duplication, or GDI where the display has
    /// no D3D11 device): frames arrive, carry the config and the layout is published. Opt-in
    /// (it captures the desktop and needs a display that can wake up):
    /// `cargo test -p rd-platform-windows -- --ignored`.
    #[test]
    #[ignore]
    fn screen_capture_of_the_primary_display() {
        let Some((w, h)) = display::native_size(0) else {
            return;
        };
        // A dark display delivers no frames; the runtime wakes it at session start too.
        let _awake = crate::power::keep_display_awake("rd-platform-windows test");
        crate::power::declare_user_activity();
        let gate = Arc::new(TestGate::default());
        let (tx, mut rx) = mpsc::channel(64);
        let geometry = Arc::new(CaptureGeometry::default());
        let mut pipeline =
            ScreenPipeline::new(EncoderPreference::Auto).with_capture_geometry(geometry.clone());
        let height = ((640.0 * f64::from(h) / f64::from(w) / 2.0).round() as u32) * 2;
        if let Err(e) = pipeline.start(video(640, height), gate.clone(), tx) {
            println!("skipping: {e:#}");
            return;
        }
        let layout = geometry.current().expect("layout published");
        assert!(layout.stretch, "{layout:?}");
        // A static desktop may send nothing: ask for a keyframe, which re-submits the image.
        gate.force.store(true, Ordering::Relaxed);
        let first = recv(&mut rx);
        assert!(first.key && first.config.is_some());
        pipeline.stop();
        assert_eq!(geometry.current(), None, "retracted on stop");
    }
}
