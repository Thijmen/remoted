//! End-to-end ACK window. Thread-safe (`&self` everywhere): `admit`/`cancel` are called from
//! the capture/encoder threads, the rest from the network task.
//!
//! IMPLEMENTATION NOTE (to be filled in): keep state in a `parking_lot::Mutex<State>`; the
//! critical sections are tiny. `admit` must be cheap (called at up to 120 Hz).

use parking_lot::Mutex;

/// Window configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowConfig {
    pub fps: u32,
    /// Extra queueing allowance on top of rtt_min, microseconds. Default 40_000.
    pub queue_target_us: u64,
    /// Lower clamp for the frame window. Default 2.
    pub min_frames: u32,
    /// Upper clamp for the frame window. Default 16.
    pub max_frames: u32,
    /// Initial target bitrate (bits/s) used for `W_bytes` until `set_target_bps` is called.
    pub target_bps: u32,
    /// RTT assumed before any sample arrives, microseconds. Default 50_000.
    pub initial_rtt_us: u64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        FlowConfig {
            fps: 60,
            queue_target_us: 40_000,
            min_frames: 2,
            max_frames: 16,
            target_bps: 8_000_000,
            initial_rtt_us: 50_000,
        }
    }
}

/// Result of a successful [`FlowGate::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    pub seq: u32,
    /// Encode this frame as an IDR (set after `request_keyframe` or a post-encode drop;
    /// consumed by the first admitted frame).
    pub force_keyframe: bool,
}

/// One frame released by an ACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckSample {
    pub seq: u32,
    /// Encoded size (0 if the frame was cancelled / never sent).
    pub bytes: usize,
    pub key: bool,
    /// `ack_time - sent_at` (0 if never sent).
    pub sent_to_ack_us: u64,
    /// Decoder time reported by the viewer (only meaningful for the acked seq itself; 0 for
    /// frames released implicitly by a cumulative ACK).
    pub decode_us: u32,
    /// Frame was sent (false = cancelled/never sent; excluded from delay statistics).
    pub was_sent: bool,
}

/// Snapshot for stats/HUD.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FlowStats {
    pub inflight_frames: u32,
    pub inflight_bytes: u64,
    pub window_frames: u32,
    pub window_bytes: u64,
    pub rtt_min_us: u64,
    /// Total frames dropped by the window since creation.
    pub drop_window_total: u64,
    /// Total frames discarded after encode since creation.
    pub drop_post_total: u64,
    /// Total frames admitted since creation.
    pub admitted_total: u64,
    pub paused: bool,
    pub last_seq: u32,
}

pub struct FlowGate {
    #[allow(dead_code)]
    inner: Mutex<State>,
}

#[allow(dead_code)]
struct State {
    cfg: FlowConfig,
    // ... in-flight deque, counters, rtt tracker, flags ...
}

impl FlowGate {
    pub fn new(cfg: FlowConfig) -> Self {
        let _ = cfg;
        todo!("rd-flow: FlowGate::new")
    }

    /// Change the frame rate used for `W_frames`.
    pub fn set_fps(&self, fps: u32) {
        let _ = fps;
        todo!()
    }

    /// Current target bitrate (used for `W_bytes`). Called when the bitrate controller changes it.
    pub fn set_target_bps(&self, bps: u32) {
        let _ = bps;
        todo!()
    }

    /// Paused gates admit nothing (viewer tab hidden).
    pub fn set_paused(&self, paused: bool) {
        let _ = paused;
        todo!()
    }

    /// Next admitted frame will be an IDR.
    pub fn request_keyframe(&self) {
        todo!()
    }

    /// Capture thread: should this captured frame be encoded? `None` = drop before encode
    /// (counted in `drop_window_total`). Always admits when nothing is in flight (unless paused).
    pub fn admit(&self, now_us: u64) -> Option<Admitted> {
        let _ = now_us;
        todo!()
    }

    /// The encoder produced no output for an admitted `seq`: remove it from the window.
    pub fn cancel(&self, seq: u32) {
        let _ = seq;
        todo!()
    }

    /// Network task: encoded frame `seq` of `bytes` was handed to the socket at `now_us`.
    pub fn on_sent(&self, seq: u32, bytes: usize, key: bool, now_us: u64) {
        let _ = (seq, bytes, key, now_us);
        todo!()
    }

    /// An already-encoded frame had to be discarded (should never happen). Removes it from the
    /// window, counts `drop_post_total`, and forces the next admitted frame to be an IDR.
    pub fn on_post_encode_drop(&self, seq: u32) {
        let _ = seq;
        todo!()
    }

    /// Cumulative ACK for `seq` (all frames with seq <= `seq`, wrapping-aware). Returns the
    /// released frames in seq order. ACKs for unknown/old seqs return an empty vec.
    pub fn on_ack(&self, seq: u32, decode_us: u32, now_us: u64) -> Vec<AckSample> {
        let _ = (seq, decode_us, now_us);
        todo!()
    }

    /// Feed an application-level RTT sample (PING/PONG) — updates rtt_min (min over last 10 s).
    pub fn on_rtt_sample(&self, rtt_us: u64, now_us: u64) {
        let _ = (rtt_us, now_us);
        todo!()
    }

    /// Age of the oldest in-flight frame (from admit time), if any. Used for stall detection
    /// (log at >1.5 s, end session at >8 s).
    pub fn oldest_inflight_age_us(&self, now_us: u64) -> Option<u64> {
        let _ = now_us;
        todo!()
    }

    /// Clear the window (new session / reconnect). Keeps seq counter and totals.
    pub fn reset(&self) {
        todo!()
    }

    pub fn stats(&self) -> FlowStats {
        todo!()
    }
}
