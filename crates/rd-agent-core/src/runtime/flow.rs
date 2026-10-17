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
    pub(crate) fn new(params: &Effective) -> Self {
        Flow {
            gate: FlowGate::new(FlowConfig {
                fps: params.fps,
                target_bps: params.bitrate.start_bps,
                ..FlowConfig::default()
            }),
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

/// Fits the requested size into the display's native size (keeping the aspect ratio), enforces
/// the minimum size and rounds down to even numbers (4:2:0 chroma).
pub(crate) fn clamp_size(width: u32, height: u32, native: (u32, u32)) -> (u32, u32) {
    let (nw, nh) = (native.0.max(2), native.1.max(2));
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (f64::from(nw) / w).min(f64::from(nh) / h).min(1.0);
    let fit = |v: f64, min: u32, max: u32| ((v * scale).round() as u32).clamp(min.min(max), max);
    let w = fit(w, MIN_WIDTH, nw);
    let h = fit(h, MIN_HEIGHT, nh);
    (w & !1, h & !1)
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

    #[test]
    fn sizes() {
        let native = (3456, 2234);
        assert_eq!(clamp_size(1728, 1118, native), (1728, 1118));
        assert_eq!(clamp_size(1727, 1117, native), (1726, 1116));
        assert_eq!(clamp_size(3456, 2234, native), (3456, 2234));
        // Too large: scaled down to fit, aspect kept.
        assert_eq!(clamp_size(6912, 4468, native), (3456, 2234));
        assert_eq!(clamp_size(7000, 1000, native), (3456, 494));
        // Too small: minimum per dimension.
        assert_eq!(clamp_size(100, 100, native), (320, 200));
        assert_eq!(clamp_size(0, 0, native), (320, 200));
        // A display smaller than the minimum wins.
        assert_eq!(clamp_size(1920, 1080, (300, 101)), (300, 100));
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
        assert!(Effective::new(&params(1728, 1118), None, &d).is_err());
        let mut p = params(1728, 1118);
        p.codec = VideoCodec::Hevc;
        assert!(Effective::new(&p, Some((3456, 2234)), &d).is_err());
        p.codec = VideoCodec::Avc;
        p.fps = 1000;
        assert_eq!(Effective::new(&p, Some((3456, 2234)), &d).unwrap().fps, 120);
    }
}
