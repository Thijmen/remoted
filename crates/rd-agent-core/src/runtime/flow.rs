//! Flow-control glue: the [`FrameGate`] the platform pipeline calls, on top of
//! [`rd_flow::FlowGate`] (ACK window) and [`rd_flow::BitrateController`], plus the stream
//! parameter rules (size clamping, bitrate limits).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use rd_flow::{BitrateConfig, BitrateController, FlowConfig, FlowGate};
use rd_proto::bin::VideoAck;
use rd_proto::control::{StreamParams, VideoCodec};

use crate::clock::now_us;
use crate::config::StreamDefaults;
use crate::platform::{Admitted, FrameGate, VideoConfig};

/// Smallest output we encode.
const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 200;
const MAX_FPS: u32 = 120;
const MIN_KBPS: u32 = 100;
const MAX_KBPS: u32 = 500_000;
/// Upper clamp of the flow window (`W_frames`). The frame sink is sized from it so that frames
/// the window admitted always fit (see `session::FRAME_SINK_CAPACITY`).
pub(crate) const MAX_WINDOW_FRAMES: u32 = 16;

/// Window + bitrate state of one session. Thread-safe: the capture/encoder threads call the
/// [`FrameGate`] methods, the network tasks the rest.
pub(crate) struct Flow {
    pub(crate) gate: FlowGate,
    pub(crate) bitrate: Mutex<BitrateController>,
    paused: AtomicBool,
    /// Frames offered to [`FrameGate::admit`] (for `cap_fps`).
    captured: AtomicU64,
}

impl Flow {
    /// A new session's flow. The first frame is a keyframe ([`FrameGate::refresh_wanted`]
    /// stays true until one is admitted, so a static screen still produces it).
    pub(crate) fn new(params: &Effective) -> Self {
        let gate = FlowGate::new(FlowConfig {
            fps: params.fps,
            target_bps: params.bitrate.start_bps,
            max_frames: MAX_WINDOW_FRAMES,
            ..FlowConfig::default()
        });
        gate.request_keyframe();
        Flow {
            gate,
            bitrate: Mutex::new(BitrateController::new(params.bitrate, now_us())),
            paused: AtomicBool::new(false),
            captured: AtomicU64::new(0),
        }
    }

    /// A new `start` on a running session: fresh bitrate controller (startup ramp again).
    pub(crate) fn restart(&self, params: &Effective) {
        self.gate.set_fps(params.fps);
        self.gate.set_target_bps(params.bitrate.start_bps);
        *self.bitrate.lock() = BitrateController::new(params.bitrate, now_us());
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        self.gate.set_paused(paused);
    }

    pub(crate) fn on_ack(&self, ack: &VideoAck) {
        let now = now_us();
        let samples = self.gate.on_ack(ack.seq, u32::from(ack.decode_us), now);
        if !samples.is_empty() {
            self.bitrate.lock().on_ack_samples(&samples, now);
        }
    }

    pub(crate) fn captured_total(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }

    pub(crate) fn target_bps(&self) -> u32 {
        self.bitrate.lock().target_bps()
    }
}

impl FrameGate for Flow {
    fn admit(&self, _capture_us: u64) -> Option<Admitted> {
        // The gate runs on our clock at admission (stall detection measures from here).
        let now = now_us();
        self.captured.fetch_add(1, Ordering::Relaxed);
        let admitted = self.gate.admit(now);
        // Frames refused while paused are not window drops.
        if !self.paused.load(Ordering::Relaxed) {
            self.bitrate.lock().on_captured(admitted.is_none(), now);
        }
        admitted
    }

    fn cancel(&self, seq: u32) {
        self.gate.cancel(seq);
    }

    fn post_encode_drop(&self, seq: u32) {
        self.gate.on_post_encode_drop(seq);
    }

    fn refresh_wanted(&self) -> bool {
        !self.paused.load(Ordering::Relaxed) && self.gate.keyframe_pending()
    }
}

/// Stream parameters after validation and clamping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Effective {
    pub(crate) display: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) bitrate: BitrateConfig,
}

impl Effective {
    /// Validates `p`; `native` is the display's pixel size (`None` = no such display).
    pub(crate) fn new(
        p: &StreamParams,
        native: Option<(u32, u32)>,
        defaults: &StreamDefaults,
    ) -> Result<Self, String> {
        if p.codec != VideoCodec::Avc {
            return Err(format!("codec {:?} is not supported (only avc)", p.codec));
        }
        let native = native.ok_or_else(|| format!("unknown display {}", p.display))?;
        let (width, height) = clamp_size(p.width, p.height, native);
        let fps = if p.fps == 0 { defaults.fps } else { p.fps }.clamp(1, MAX_FPS);
        Ok(Effective {
            display: p.display,
            width,
            height,
            fps,
            bitrate: bitrate_config(p, defaults),
        })
    }

    pub(crate) fn video_config(&self, bitrate_bps: u32) -> VideoConfig {
        VideoConfig {
            display: self.display,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_bps,
        }
    }

    /// Changes that need a pipeline reconfigure (and therefore a new epoch).
    pub(crate) fn video_differs(&self, other: &Effective) -> bool {
        (self.display, self.width, self.height, self.fps)
            != (other.display, other.width, other.height, other.fps)
    }
}

/// The output size for a requested `width`x`height` on a display of `native` pixels. The
/// result always has the **display's** aspect ratio: the request is a bounding box, and the
/// display is scaled to the largest size that fits in it (never up), then raised to the minimum
/// size if needed, and rounded to even numbers (4:2:0 chroma).
///
/// The display's own aspect matters: the capture scales the display into the output frame
/// keeping its aspect, so any other aspect would letterbox it inside the frame, and the viewer
/// (which maps the pointer over the whole frame) would click next to where it points.
pub(crate) fn clamp_size(width: u32, height: u32, native: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = (native.0.max(2), native.1.max(2));
    let (fw, fh) = (f64::from(nw), f64::from(nh));
    let (bw, bh) = (width.max(1), height.max(1));
    // Largest scale that fits the bounding box, and the smallest that meets the minimums.
    let fit = (f64::from(bw) / fw).min(f64::from(bh) / fh);
    let floor = (f64::from(MIN_WIDTH) / fw).max(f64::from(MIN_HEIGHT) / fh);
    let scale = fit.max(floor).min(1.0);
    // Rounded to the nearest even number, but never beyond the display or (unless the minimum
    // size forced a larger scale) the requested box.
    let request_bound = scale <= fit;
    let even = |v: f64, native: u32, requested: u32| {
        let mut limit = native;
        if request_bound {
            limit = limit.min(requested);
        }
        let rounded = ((v / 2.0).round() as u32).saturating_mul(2);
        rounded.min(limit & !1).max(2)
    };
    (even(fw * scale, nw, bw), even(fh * scale, nh, bh))
}

/// Bitrate limits from `start`: `min <= start <= max`, all within sane bounds. Zero values fall
/// back to the agent's defaults.
pub(crate) fn bitrate_config(p: &StreamParams, defaults: &StreamDefaults) -> BitrateConfig {
    let max_kbps = if p.max_kbps == 0 {
        defaults.max_kbps
    } else {
        p.max_kbps
    }
    .clamp(MIN_KBPS, MAX_KBPS);
    let min_kbps = p.min_kbps.clamp(MIN_KBPS, max_kbps);
    let start_kbps = if p.start_kbps == 0 {
        max_kbps
    } else {
        p.start_kbps
    }
    .clamp(min_kbps, max_kbps);
    BitrateConfig {
        start_bps: start_kbps * 1000,
        min_bps: min_kbps * 1000,
        max_bps: max_kbps * 1000,
        adaptive: p.adaptive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(width: u32, height: u32) -> StreamParams {
        StreamParams {
            width,
            height,
            fps: 60,
            codec: VideoCodec::Avc,
            start_kbps: 8000,
            min_kbps: 1000,
            max_kbps: 20000,
            adaptive: true,
            audio: false,
            display: 0,
        }
    }

    /// Width/height of `size` vs the display's, as a relative error.
    fn aspect_error(size: (u32, u32), native: (u32, u32)) -> f64 {
        let a = f64::from(size.0) / f64::from(size.1);
        let n = f64::from(native.0) / f64::from(native.1);
        (a / n - 1.0).abs()
    }

    #[test]
    fn sizes() {
        // The built-in 16" panel (3456x2234 px, 16:10.3): the presets fit it.
        let native = (3456, 2234);
        assert_eq!(clamp_size(1728, 1118, native), (1728, 1118));
        assert_eq!(clamp_size(1727, 1117, native), (1726, 1116));
        assert_eq!(clamp_size(3456, 2234, native), (3456, 2234));
        assert_eq!(clamp_size(1280, 828, native), (1280, 828));
        // Too large: the whole display, never upscaled.
        assert_eq!(clamp_size(6912, 4468, native), (3456, 2234));
        assert_eq!(clamp_size(7001, 4469, native), (3456, 2234));
        // A box of another shape: the display's aspect, fitted inside it.
        assert_eq!(clamp_size(7000, 1000, native), (1548, 1000));
        assert_eq!(clamp_size(1000, 7000, native), (1000, 646));
        // Too small: raised to the minimum size, aspect kept.
        assert_eq!(clamp_size(100, 100, native), (320, 206));
        assert_eq!(clamp_size(0, 0, native), (320, 206));
        // A display smaller than the minimum wins.
        assert_eq!(clamp_size(1920, 1080, (300, 101)), (300, 100));
    }

    #[test]
    fn sizes_on_a_16_9_display() {
        // External monitor / HDMI dummy plug at 1920x1080 pt, HiDPI and not.
        for native in [(3840, 2160), (1920, 1080)] {
            // The default preset is a bounding box: 1728 wide, the display's 16:9 height.
            assert_eq!(clamp_size(1728, 1118, native), (1728, 972), "{native:?}");
            assert_eq!(clamp_size(1280, 828, native), (1280, 720), "{native:?}");
            assert_eq!(clamp_size(100, 100, native), (356, 200), "{native:?}");
        }
        assert_eq!(clamp_size(2592, 1676, (3840, 2160)), (2592, 1458));
        assert_eq!(clamp_size(3456, 2234, (3840, 2160)), (3456, 1944));
        assert_eq!(clamp_size(3456, 2234, (1920, 1080)), (1920, 1080));
        // Ultrawide and portrait.
        assert_eq!(clamp_size(1728, 1118, (3440, 1440)), (1728, 724));
        assert_eq!(clamp_size(1728, 1118, (1080, 1920)), (628, 1118));
        for native in [
            (3840, 2160),
            (1920, 1080),
            (3440, 1440),
            (1080, 1920),
            (2560, 1600),
        ] {
            for (w, h) in [
                (1728, 1118),
                (2592, 1676),
                (1280, 828),
                (1536, 750),
                (640, 480),
            ] {
                let size = clamp_size(w, h, native);
                assert!(
                    size.0.is_multiple_of(2) && size.1.is_multiple_of(2),
                    "{size:?}"
                );
                assert!(size.0 <= native.0 && size.1 <= native.1, "{size:?}");
                // Inside the requested box, unless the minimum size had to win.
                if size.0 > MIN_WIDTH && size.1 > MIN_HEIGHT {
                    assert!(size.0 <= w && size.1 <= h, "{size:?} outside {w}x{h}");
                }
                // Within the rounding of one pixel per dimension.
                assert!(aspect_error(size, native) < 0.005, "{size:?} on {native:?}");
            }
        }
    }

    #[test]
    fn bitrates() {
        let d = StreamDefaults::default();
        let b = bitrate_config(&params(1, 1), &d);
        assert_eq!(
            (b.start_bps, b.min_bps, b.max_bps),
            (8_000_000, 1_000_000, 20_000_000)
        );
        let mut p = params(1, 1);
        p.start_kbps = 50_000;
        p.min_kbps = 0;
        p.max_kbps = 0;
        let b = bitrate_config(&p, &d);
        assert_eq!(
            (b.start_bps, b.min_bps, b.max_bps),
            (40_000_000, 100_000, 40_000_000)
        );
        p.max_kbps = u32::MAX;
        p.min_kbps = u32::MAX;
        let b = bitrate_config(&p, &d);
        assert_eq!(b.max_bps, MAX_KBPS * 1000);
        assert_eq!(b.min_bps, MAX_KBPS * 1000);
    }

    #[test]
    fn effective() {
        let d = StreamDefaults::default();
        let e = Effective::new(&params(1728, 1118), Some((3456, 2234)), &d).unwrap();
        assert_eq!((e.width, e.height, e.fps), (1728, 1118, 60));
        // On a 16:9 display the preset becomes 16:9, and that is what the pipeline (and from
        // there video_config) gets.
        let e = Effective::new(&params(1728, 1118), Some((3840, 2160)), &d).unwrap();
        let cfg = e.video_config(8_000_000);
        assert_eq!((cfg.width, cfg.height), (1728, 972));
        assert!(Effective::new(&params(1728, 1118), None, &d).is_err());
        let mut p = params(1728, 1118);
        p.codec = VideoCodec::Hevc;
        assert!(Effective::new(&p, Some((3456, 2234)), &d).is_err());
        p.codec = VideoCodec::Avc;
        p.fps = 1000;
        assert_eq!(Effective::new(&p, Some((3456, 2234)), &d).unwrap().fps, 120);
    }

    #[test]
    fn refresh_wanted_follows_keyframe_requests() {
        let d = StreamDefaults::default();
        let e = Effective::new(&params(1728, 1118), Some((3456, 2234)), &d).unwrap();
        let flow = Flow::new(&e);
        // A new session wants its first keyframe.
        assert!(flow.refresh_wanted());
        let first = flow.admit(now_us()).unwrap();
        assert!(first.force_keyframe);
        assert!(!flow.refresh_wanted(), "false once a frame is admitted");
        flow.gate.on_ack(first.seq, 0, now_us());
        // keyframe_req / resume.
        flow.gate.request_keyframe();
        assert!(flow.refresh_wanted());
        flow.set_paused(true);
        assert!(!flow.refresh_wanted(), "not while paused");
        flow.set_paused(false);
        assert!(flow.refresh_wanted());
        assert!(flow.admit(now_us()).unwrap().force_keyframe);
        assert!(!flow.refresh_wanted());
    }
}
