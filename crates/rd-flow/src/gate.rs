//! End-to-end ACK window. Thread-safe (`&self` everywhere): `admit`/`cancel` are called from
//! the capture/encoder threads, the rest from the network task.
//!
//! State lives in one `parking_lot::Mutex<State>`; every critical section is a handful of
//! integer operations on a deque of at most `max_frames` entries, so `admit` stays cheap even
//! at 120 Hz.

use std::collections::VecDeque;

use parking_lot::Mutex;

use crate::rtt::RttTracker;

/// `rtt_min` is the minimum RTT over this window.
const RTT_WINDOW_US: u64 = 10_000_000;
/// The encode term is the median of this many recent non-key encode times.
const ENCODE_SAMPLES: usize = 64;

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
    /// Encode latency assumed before the first [`FlowGate::on_encode_time`] sample,
    /// microseconds. Default 10_000. Use 0 when there is no encoder (synthetic frames).
    pub initial_encode_us: u64,
    /// Upper clamp of the encode term, microseconds; it is also never more than one frame
    /// interval (`1e6 / fps`), so a backlogged encoder cannot widen the window. Default 25_000.
    pub max_encode_us: u64,
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
            initial_encode_us: 10_000,
            max_encode_us: 25_000,
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
    /// Bytes of in-flight frames that have been handed to the socket (frames still in the
    /// encoder are not included).
    pub inflight_bytes: u64,
    pub window_frames: u32,
    pub window_bytes: u64,
    pub rtt_min_us: u64,
    /// Encode term of the window horizon: median of the recent non-key encode times, clamped
    /// to `min(max_encode_us, 1e6 / fps)` (`initial_encode_us` before the first sample).
    pub encode_us: u64,
    /// Total frames dropped by the window since creation (frames refused while paused are not
    /// counted).
    pub drop_window_total: u64,
    /// Total frames discarded after encode since creation.
    pub drop_post_total: u64,
    /// Total frames admitted since creation.
    pub admitted_total: u64,
    pub paused: bool,
    /// Last admitted seq (0 before the first admit).
    pub last_seq: u32,
}

pub struct FlowGate {
    inner: Mutex<State>,
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    seq: u32,
    admitted_at_us: u64,
    /// Admitted with `force_keyframe` (it carries a requested IDR).
    forced: bool,
    sent: Option<Sent>,
}

#[derive(Debug, Clone, Copy)]
struct Sent {
    at_us: u64,
    bytes: usize,
    key: bool,
}

struct State {
    cfg: FlowConfig,
    next_seq: u32,
    last_seq: u32,
    /// Admitted, not yet released frames in seq order.
    inflight: VecDeque<InFlight>,
    /// Sum of `bytes` over the sent frames in `inflight`.
    sent_bytes: u64,
    /// Number of frames in `inflight` that have not been sent yet (still encoding).
    unsent: u32,
    rtt: RttTracker,
    /// Recent encode times; survive `reset()` (the encoder did not change).
    encode: EncodeTimes,
    /// Latest clock value seen by any call; `stats()` evaluates the window at this time.
    now_us: u64,
    paused: bool,
    force_keyframe: bool,
    drop_window_total: u64,
    drop_post_total: u64,
    admitted_total: u64,
}

/// `a` precedes or equals `b` in wrapping u32 sequence space.
fn seq_le(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) >= 0
}

/// The seq after `seq`. 0 is skipped so that seqs are never 0 (0 = "none" in stats/ACKs).
fn next_seq(seq: u32) -> u32 {
    match seq.wrapping_add(1) {
        0 => 1,
        n => n,
    }
}

impl FlowGate {
    pub fn new(cfg: FlowConfig) -> Self {
        Self::with_first_seq(cfg, 1)
    }

    /// Like [`FlowGate::new`] but the first admitted frame gets `first_seq` (0 is mapped to 1).
    /// Mainly for testing sequence wrap-around.
    pub fn with_first_seq(cfg: FlowConfig, first_seq: u32) -> Self {
        let first_seq = first_seq.max(1);
        FlowGate {
            inner: Mutex::new(State {
                cfg,
                next_seq: first_seq,
                last_seq: 0,
                inflight: VecDeque::with_capacity(cfg.max_frames as usize),
                sent_bytes: 0,
                unsent: 0,
                rtt: RttTracker::new(RTT_WINDOW_US),
                encode: EncodeTimes::default(),
                now_us: 0,
                paused: false,
                force_keyframe: false,
                drop_window_total: 0,
                drop_post_total: 0,
                admitted_total: 0,
            }),
        }
    }

    /// Change the frame rate used for `W_frames`.
    pub fn set_fps(&self, fps: u32) {
        self.inner.lock().cfg.fps = fps;
    }

    /// Current target bitrate (used for `W_bytes`). Called when the bitrate controller changes it.
    pub fn set_target_bps(&self, bps: u32) {
        self.inner.lock().cfg.target_bps = bps;
    }

    /// Paused gates admit nothing (viewer tab hidden).
    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().paused = paused;
    }

    /// Next admitted frame will be an IDR.
    pub fn request_keyframe(&self) {
        self.inner.lock().force_keyframe = true;
    }

    /// An IDR has been requested (and not paused) but no frame has been admitted since, and
    /// the window has room for one. A static screen produces no captures, so the pipeline
    /// should then re-submit its most recent frame. Turns false once a frame is admitted.
    pub fn keyframe_pending(&self) -> bool {
        let s = self.inner.lock();
        s.force_keyframe && !s.paused && (s.inflight.is_empty() || s.window_open())
    }

    /// A keyframe the viewer has not seen yet is on its way, so a keyframe request arriving now
    /// is already answered by it: one was requested but not admitted yet (also while paused),
    /// or the frame carrying it is still in the encoder or the agent's queues, or it was handed
    /// to the socket less than `rtt_min` ago (a request arriving that soon was sent before the
    /// viewer can have received the keyframe). `rtt_min` counts only once measured.
    pub fn keyframe_on_its_way(&self, now_us: u64) -> bool {
        let mut s = self.inner.lock();
        s.observe(now_us);
        if s.force_keyframe {
            return true;
        }
        let crossing_us = s.rtt.min(s.now_us).unwrap_or(0);
        s.inflight.iter().any(|f| {
            f.forced
                && f.sent
                    .is_none_or(|sent| now_us.saturating_sub(sent.at_us) < crossing_us)
        })
    }

    /// Capture thread: should this captured frame be encoded? `None` = drop before encode
    /// (counted in `drop_window_total`, except while paused). Always admits when nothing is in
    /// flight (unless paused).
    pub fn admit(&self, now_us: u64) -> Option<Admitted> {
        let mut s = self.inner.lock();
        s.observe(now_us);
        if s.paused {
            return None;
        }
        if !s.inflight.is_empty() && !s.window_open() {
            s.drop_window_total += 1;
            return None;
        }
        let seq = s.next_seq;
        s.next_seq = next_seq(seq);
        s.last_seq = seq;
        s.admitted_total += 1;
        s.unsent += 1;
        let force_keyframe = std::mem::take(&mut s.force_keyframe);
        s.inflight.push_back(InFlight {
            seq,
            admitted_at_us: now_us,
            forced: force_keyframe,
            sent: None,
        });
        Some(Admitted {
            seq,
            force_keyframe,
        })
    }

    /// The encoder produced no output for an admitted `seq`: remove it from the window. If that
    /// frame was to carry a requested IDR, the request is re-armed for the next frame.
    pub fn cancel(&self, seq: u32) {
        let mut s = self.inner.lock();
        if s.remove(seq).is_some_and(|f| f.forced && f.sent.is_none()) {
            s.force_keyframe = true;
        }
    }

    /// Network task: encoded frame `seq` of `bytes` was handed to the socket at `now_us`.
    pub fn on_sent(&self, seq: u32, bytes: usize, key: bool, now_us: u64) {
        let mut s = self.inner.lock();
        s.observe(now_us);
        let Some(frame) = s.inflight.iter_mut().find(|f| f.seq == seq) else {
            return;
        };
        if frame.sent.is_some() {
            return;
        }
        frame.sent = Some(Sent {
            at_us: now_us,
            bytes,
            key,
        });
        s.unsent -= 1;
        s.sent_bytes += bytes as u64;
    }

    /// How long the encoder took for one frame: submit to output, as measured by the platform
    /// (rd-agent-core's `EncodedFrame::encode_us`). It must not include queueing after the
    /// encoder (the agent's own send queues), or the window would grow under congestion.
    /// Keyframes are ignored. The median of the last 64 samples sizes the window horizon (see
    /// [`FlowStats::encode_us`]).
    pub fn on_encode_time(&self, encode_us: u32, key: bool) {
        if !key {
            self.inner.lock().encode.push(encode_us);
        }
    }

    /// An already-encoded frame had to be discarded (should never happen). Removes it from the
    /// window, counts `drop_post_total`, and forces the next admitted frame to be an IDR.
    pub fn on_post_encode_drop(&self, seq: u32) {
        let mut s = self.inner.lock();
        // The reference chain is broken whether or not the frame was still tracked.
        s.remove(seq);
        s.drop_post_total += 1;
        s.force_keyframe = true;
    }

    /// Cumulative ACK for `seq` (all frames with seq <= `seq`, wrapping-aware). Returns the
    /// released frames in seq order. ACKs for unknown/old seqs return an empty vec.
    pub fn on_ack(&self, seq: u32, decode_us: u32, now_us: u64) -> Vec<AckSample> {
        let mut s = self.inner.lock();
        s.observe(now_us);
        // An ACK beyond the newest admitted frame cannot be genuine; releasing everything on it
        // would open the window wide.
        if s.admitted_total == 0 || !seq_le(seq, s.last_seq) {
            return Vec::new();
        }
        let mut samples = Vec::new();
        while let Some(&frame) = s.inflight.front() {
            if !seq_le(frame.seq, seq) {
                break;
            }
            s.inflight.pop_front();
            let sample = match frame.sent {
                Some(sent) => {
                    s.sent_bytes -= sent.bytes as u64;
                    AckSample {
                        seq: frame.seq,
                        bytes: sent.bytes,
                        key: sent.key,
                        sent_to_ack_us: now_us.saturating_sub(sent.at_us),
                        decode_us: if frame.seq == seq { decode_us } else { 0 },
                        was_sent: true,
                    }
                }
                None => {
                    s.unsent -= 1;
                    AckSample {
                        seq: frame.seq,
                        bytes: 0,
                        key: false,
                        sent_to_ack_us: 0,
                        decode_us: 0,
                        was_sent: false,
                    }
                }
            };
            samples.push(sample);
        }
        samples
    }

    /// Feed an application-level RTT sample (PING/PONG) — updates rtt_min (min over last 10 s).
    pub fn on_rtt_sample(&self, rtt_us: u64, now_us: u64) {
        let mut s = self.inner.lock();
        s.observe(now_us);
        s.rtt.add(rtt_us, now_us);
    }

    /// Age of the oldest in-flight frame (from admit time), if any. Used for stall detection
    /// (log at >1.5 s, end session at >8 s).
    pub fn oldest_inflight_age_us(&self, now_us: u64) -> Option<u64> {
        let s = self.inner.lock();
        s.inflight
            .front()
            .map(|f| now_us.saturating_sub(f.admitted_at_us))
    }

    /// Clear the window (new session / reconnect). Keeps seq counter and totals.
    ///
    /// Also forgets the RTT history (the path may have changed), un-pauses, and forces the next
    /// admitted frame to be an IDR (the receiver starts without a reference frame). The encode
    /// times are kept (same encoder).
    pub fn reset(&self) {
        let mut s = self.inner.lock();
        s.inflight.clear();
        s.sent_bytes = 0;
        s.unsent = 0;
        s.rtt.clear();
        s.paused = false;
        s.force_keyframe = true;
    }

    pub fn stats(&self) -> FlowStats {
        let s = self.inner.lock();
        let horizon = s.horizon_us();
        FlowStats {
            inflight_frames: s.inflight.len() as u32,
            inflight_bytes: s.sent_bytes,
            window_frames: s.window_frames(horizon),
            window_bytes: s.window_bytes(horizon),
            rtt_min_us: s.rtt_min_us(),
            encode_us: s.encode_term_us(),
            drop_window_total: s.drop_window_total,
            drop_post_total: s.drop_post_total,
            admitted_total: s.admitted_total,
            paused: s.paused,
            last_seq: s.last_seq,
        }
    }
}

impl State {
    fn observe(&mut self, now_us: u64) {
        self.now_us = self.now_us.max(now_us);
    }

    fn rtt_min_us(&self) -> u64 {
        self.rtt.min(self.now_us).unwrap_or(self.cfg.initial_rtt_us)
    }

    /// The encode term: median of the recent non-key encode times (`initial_encode_us` before
    /// the first sample), at most `max_encode_us` and one frame interval.
    fn encode_term_us(&self) -> u64 {
        let frame_us = 1_000_000 / u64::from(self.cfg.fps.max(1));
        let cap = self.cfg.max_encode_us.min(frame_us);
        self.encode
            .median
            .map_or(self.cfg.initial_encode_us, u64::from)
            .min(cap)
    }

    /// `rtt_min + encode + queue_target`, the delay budget the window is sized for. Frames
    /// count from admission, so the time they spend in the encoder must fit in it too.
    fn horizon_us(&self) -> u64 {
        self.rtt_min_us()
            .saturating_add(self.encode_term_us())
            .saturating_add(self.cfg.queue_target_us)
    }

    /// `W_frames = clamp(ceil(fps * horizon) + 1, min_frames, max_frames)`.
    fn window_frames(&self, horizon_us: u64) -> u32 {
        let frames = u64::from(self.cfg.fps)
            .saturating_mul(horizon_us)
            .div_ceil(1_000_000)
            .saturating_add(1);
        let lo = self.cfg.min_frames;
        let hi = self.cfg.max_frames.max(lo);
        frames.clamp(u64::from(lo), u64::from(hi)) as u32
    }

    /// `W_bytes = target_bps / 8 * horizon`.
    fn window_bytes(&self, horizon_us: u64) -> u64 {
        u64::from(self.cfg.target_bps).saturating_mul(horizon_us) / 8_000_000
    }

    /// Room for one more frame? Frames still in the encoder count with the average frame size
    /// at the target bitrate, since their real size is not known yet.
    fn window_open(&self) -> bool {
        let horizon = self.horizon_us();
        if self.inflight.len() as u64 >= u64::from(self.window_frames(horizon)) {
            return false;
        }
        let expected_frame = u64::from(self.cfg.target_bps) / 8 / u64::from(self.cfg.fps.max(1));
        let bytes = self.sent_bytes + u64::from(self.unsent) * expected_frame;
        bytes < self.window_bytes(horizon)
    }

    /// Drop `seq` from the window if it is still in flight; returns the removed frame.
    fn remove(&mut self, seq: u32) -> Option<InFlight> {
        let idx = self.inflight.iter().position(|f| f.seq == seq)?;
        let frame = self.inflight.remove(idx)?;
        match frame.sent {
            Some(sent) => self.sent_bytes -= sent.bytes as u64,
            None => self.unsent -= 1,
        }
        Some(frame)
    }
}

/// Ring of the last [`ENCODE_SAMPLES`] encode times with a cached median.
#[derive(Debug, Clone, Copy)]
struct EncodeTimes {
    ring: [u32; ENCODE_SAMPLES],
    len: usize,
    next: usize,
    median: Option<u32>,
}

impl Default for EncodeTimes {
    fn default() -> Self {
        EncodeTimes {
            ring: [0; ENCODE_SAMPLES],
            len: 0,
            next: 0,
            median: None,
        }
    }
}

impl EncodeTimes {
    /// O(n) on a 64-entry copy (~100 ns), once per encoded frame.
    fn push(&mut self, us: u32) {
        self.ring[self.next] = us;
        self.next = (self.next + 1) % ENCODE_SAMPLES;
        self.len = (self.len + 1).min(ENCODE_SAMPLES);
        let mut samples = self.ring;
        let live = &mut samples[..self.len];
        let mid = live.len() / 2;
        self.median = Some(*live.select_nth_unstable(mid).1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;

    /// 60 fps, 8 Mbps, 50 ms assumed RTT, no encode term: W_frames = ceil(60 * 0.09) + 1 = 7,
    /// W_bytes = 1 MB/s * 0.09 s = 90_000.
    fn gate() -> FlowGate {
        FlowGate::new(no_encoder())
    }

    fn no_encoder() -> FlowConfig {
        FlowConfig {
            initial_encode_us: 0,
            ..FlowConfig::default()
        }
    }

    fn admit_seq(g: &FlowGate, now: u64) -> u32 {
        g.admit(now).expect("admitted").seq
    }

    #[test]
    fn initial_stats() {
        let s = gate().stats();
        assert_eq!(s.inflight_frames, 0);
        assert_eq!(s.window_frames, 7);
        assert_eq!(s.window_bytes, 90_000);
        assert_eq!(s.rtt_min_us, 50_000);
        assert_eq!(s.encode_us, 0);
        assert_eq!(s.last_seq, 0);
        assert!(!s.paused);
        // The default assumes 10 ms of encode latency until measured: 100 ms horizon.
        let s = FlowGate::new(FlowConfig::default()).stats();
        assert_eq!(s.encode_us, 10_000);
        assert_eq!((s.window_frames, s.window_bytes), (7, 100_000));
    }

    #[test]
    fn encode_time_widens_the_window() {
        let g = FlowGate::new(FlowConfig {
            fps: 50,
            ..no_encoder()
        });
        g.on_rtt_sample(20 * MS, 0);
        // 20 ms RTT + 40 ms queue target: ceil(50 * 0.06) + 1 = 4 frames, 60 kB.
        let base = g.stats();
        assert_eq!((base.window_frames, base.window_bytes), (4, 60_000));
        for _ in 0..10 {
            g.on_encode_time(18_000, false);
        }
        let s = g.stats();
        assert_eq!(s.encode_us, 18_000);
        // W grows by ceil(fps * enc) = ceil(0.9) = 1 frame, and by target/8 * enc bytes.
        assert_eq!(s.window_frames, base.window_frames + 1);
        assert_eq!(s.window_bytes, base.window_bytes + 18_000);
        // Admission follows: 5 frames fit now (4 before).
        for i in 0..5 {
            assert!(g.admit(MS + i).is_some(), "frame {i}");
            g.on_sent(i as u32 + 1, 100, false, MS + i);
        }
        assert_eq!(g.admit(MS + 10), None);
    }

    #[test]
    fn encode_term_is_a_clamped_median_of_delta_frames() {
        let g = FlowGate::new(FlowConfig::default());
        // Keyframes take longer and are rare: not samples.
        g.on_encode_time(40_000, true);
        assert_eq!(g.stats().encode_us, 10_000, "initial value until a sample");
        for us in [9_000, 30_000, 8_000] {
            g.on_encode_time(us, false);
        }
        assert_eq!(g.stats().encode_us, 9_000, "median");
        // Only the last 64 samples count.
        for _ in 0..64 {
            g.on_encode_time(30_000, false);
        }
        for _ in 0..33 {
            g.on_encode_time(5_000, false);
        }
        assert_eq!(g.stats().encode_us, 5_000);
        // A backlogged encoder cannot widen the window beyond max_encode_us ...
        let g = FlowGate::new(FlowConfig {
            fps: 30,
            ..FlowConfig::default()
        });
        for _ in 0..64 {
            g.on_encode_time(200_000, false);
        }
        assert_eq!(g.stats().encode_us, 25_000);
        // ... nor beyond one frame interval.
        g.set_fps(120);
        assert_eq!(g.stats().encode_us, 8_333);
        g.set_fps(0);
        assert_eq!(g.stats().encode_us, 25_000, "fps 0 must not divide by zero");
        // The samples survive a reset (same encoder, new viewer).
        g.set_fps(30);
        g.reset();
        assert_eq!(g.stats().encode_us, 25_000);
    }

    #[test]
    fn keyframe_on_its_way_until_sent_plus_one_rtt() {
        let g = gate();
        assert!(!g.keyframe_on_its_way(0));
        g.request_keyframe();
        assert!(g.keyframe_on_its_way(0), "requested, not admitted");
        let plain = admit_seq(&g, 0);
        assert_ne!(plain, 0);
        let forced = g.admit(1).unwrap();
        assert!(
            !forced.force_keyframe,
            "consumed by the first admitted frame"
        );
        // The first frame carried it.
        assert!(g.keyframe_on_its_way(2), "in the encoder");
        g.on_sent(plain, 100, false, 3);
        g.on_sent(1, 5_000, true, 3);
        // No RTT measured yet: once sent, a later request may be a new one.
        assert!(!g.keyframe_on_its_way(4));
        // With a 30 ms RTT, a request arriving within 30 ms of the send crossed the keyframe.
        g.on_rtt_sample(30 * MS, 5);
        assert!(g.keyframe_on_its_way(20 * MS));
        assert!(!g.keyframe_on_its_way(31 * MS));
        g.on_ack(2, 0, 40 * MS);
        assert!(!g.keyframe_on_its_way(41 * MS));
        // Requested while paused: still on its way (produced on resume).
        g.set_paused(true);
        g.request_keyframe();
        assert!(g.keyframe_on_its_way(42 * MS));
    }

    #[test]
    fn cancelled_keyframe_is_requested_again() {
        let g = gate();
        g.request_keyframe();
        let a = g.admit(0).unwrap();
        assert!(a.force_keyframe);
        assert!(!g.keyframe_pending());
        // The encoder produced nothing for it: the next frame must be the IDR.
        g.cancel(a.seq);
        assert!(g.keyframe_pending());
        assert!(g.admit(1).unwrap().force_keyframe);
    }

    #[test]
    fn keyframe_pending_until_a_frame_is_admitted() {
        let g = FlowGate::new(FlowConfig {
            max_frames: 2,
            ..no_encoder()
        });
        assert!(!g.keyframe_pending());
        g.admit(0);
        g.admit(1);
        g.request_keyframe();
        assert!(
            !g.keyframe_pending(),
            "no room: a re-submitted frame would be refused"
        );
        g.on_ack(1, 0, 2);
        assert!(g.keyframe_pending());
        g.set_paused(true);
        assert!(!g.keyframe_pending(), "paused");
        g.set_paused(false);
        assert!(g.admit(3).unwrap().force_keyframe);
        assert!(
            !g.keyframe_pending(),
            "turns false once a frame is admitted"
        );
    }

    #[test]
    fn seqs_start_at_one_and_increase() {
        let g = gate();
        assert_eq!(
            g.admit(0),
            Some(Admitted {
                seq: 1,
                force_keyframe: false
            })
        );
        assert_eq!(admit_seq(&g, 1), 2);
        assert_eq!(admit_seq(&g, 2), 3);
        let s = g.stats();
        assert_eq!((s.last_seq, s.admitted_total, s.inflight_frames), (3, 3, 3));
    }

    #[test]
    fn frame_window_limits_admission() {
        let g = gate();
        for i in 0..7 {
            let seq = admit_seq(&g, i);
            g.on_sent(seq, 100, false, i);
        }
        assert_eq!(g.admit(7), None);
        assert_eq!(g.admit(8), None);
        assert_eq!(g.stats().drop_window_total, 2);
        assert_eq!(g.on_ack(1, 0, 9).len(), 1);
        assert!(g.admit(10).is_some());
    }

    #[test]
    fn byte_window_limits_admission() {
        let g = gate();
        let a = admit_seq(&g, 0);
        g.on_sent(a, 60_000, false, 1);
        let b = admit_seq(&g, 2);
        g.on_sent(b, 30_000, false, 3);
        // 90_000 bytes in flight = W_bytes.
        assert_eq!(g.admit(4), None);
        assert_eq!(g.stats().inflight_bytes, 90_000);
        g.on_ack(a, 0, 5);
        assert!(g.admit(6).is_some());
    }

    #[test]
    fn frames_in_the_encoder_count_as_average_frames() {
        // 8 Mbps / 60 fps = 16_666 bytes per frame; 6 unsent frames = 99_996 >= 90_000.
        let g = gate();
        for i in 0..6 {
            g.admit(i).expect("admitted");
        }
        assert_eq!(g.admit(6), None);
    }

    #[test]
    fn huge_keyframe_never_deadlocks() {
        let g = gate();
        let key = admit_seq(&g, 0);
        g.on_sent(key, 2_000_000, true, 1);
        for t in 2..50 {
            assert_eq!(g.admit(t), None);
        }
        let samples = g.on_ack(key, 4_000, 100 * MS);
        assert_eq!(samples.len(), 1);
        assert!(samples[0].key);
        // Nothing in flight: always admitted, however small the window.
        g.set_target_bps(1);
        assert!(g.admit(101 * MS).is_some());
    }

    #[test]
    fn paused_gate_admits_nothing() {
        let g = gate();
        g.request_keyframe();
        g.set_paused(true);
        assert_eq!(g.admit(0), None);
        let s = g.stats();
        assert!(s.paused);
        assert_eq!(s.drop_window_total, 0, "pause drops are not window drops");
        g.set_paused(false);
        // The keyframe request survived the pause.
        assert_eq!(
            g.admit(1),
            Some(Admitted {
                seq: 1,
                force_keyframe: true
            })
        );
    }

    #[test]
    fn keyframe_request_is_consumed_once() {
        let g = gate();
        g.request_keyframe();
        g.request_keyframe();
        assert!(g.admit(0).unwrap().force_keyframe);
        assert!(!g.admit(1).unwrap().force_keyframe);
        // A refused frame does not consume it.
        let g = FlowGate::new(FlowConfig {
            max_frames: 2,
            ..no_encoder()
        });
        g.admit(0);
        g.admit(1);
        g.request_keyframe();
        assert_eq!(g.admit(2), None);
        g.on_ack(2, 0, 3);
        assert!(g.admit(4).unwrap().force_keyframe);
    }

    #[test]
    fn cancel_frees_the_slot() {
        let g = FlowGate::new(FlowConfig {
            max_frames: 2,
            ..FlowConfig::default()
        });
        let a = admit_seq(&g, 0);
        let b = admit_seq(&g, 1);
        assert_eq!(g.admit(2), None);
        g.cancel(a);
        assert_eq!(g.stats().inflight_frames, 1);
        let c = admit_seq(&g, 3);
        // Cancelled frames produce no ACK sample; the gap is fine for a cumulative ACK.
        g.on_sent(b, 100, false, 4);
        g.on_sent(c, 100, false, 4);
        let seqs: Vec<u32> = g.on_ack(c, 0, 5).iter().map(|s| s.seq).collect();
        assert_eq!(seqs, [b, c]);
        // Unknown / already released seqs are ignored.
        g.cancel(a);
        g.cancel(999);
        assert_eq!(g.stats().inflight_frames, 0);
        assert!(
            !g.admit(6).unwrap().force_keyframe,
            "cancel does not force an IDR"
        );
    }

    #[test]
    fn cancel_after_send_releases_bytes() {
        let g = gate();
        let a = admit_seq(&g, 0);
        g.on_sent(a, 5_000, false, 1);
        g.cancel(a);
        let s = g.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (0, 0));
    }

    #[test]
    fn post_encode_drop_forces_keyframe() {
        let g = gate();
        let a = admit_seq(&g, 0);
        let b = admit_seq(&g, 1);
        g.on_sent(a, 1_000, false, 2);
        g.on_post_encode_drop(a);
        let s = g.stats();
        assert_eq!(
            (s.inflight_frames, s.inflight_bytes, s.drop_post_total),
            (1, 0, 1)
        );
        assert_eq!(g.admit(3).map(|x| x.force_keyframe), Some(true));
        // Even for a seq that is no longer tracked.
        g.on_post_encode_drop(b + 100);
        assert_eq!(g.stats().drop_post_total, 2);
        assert_eq!(g.admit(4).map(|x| x.force_keyframe), Some(true));
    }

    #[test]
    fn cumulative_ack_releases_in_order() {
        let g = gate();
        let seqs: Vec<u32> = (0..4).map(|i| admit_seq(&g, i * MS)).collect();
        g.on_sent(seqs[0], 1_000, true, 10 * MS);
        g.on_sent(seqs[1], 2_000, false, 20 * MS);
        // seqs[2] never sent (encoder still busy / dropped without cancel).
        g.on_sent(seqs[3], 4_000, false, 30 * MS);
        let samples = g.on_ack(seqs[3], 3_500, 100 * MS);
        assert_eq!(
            samples,
            vec![
                AckSample {
                    seq: 1,
                    bytes: 1_000,
                    key: true,
                    sent_to_ack_us: 90 * MS,
                    decode_us: 0,
                    was_sent: true
                },
                AckSample {
                    seq: 2,
                    bytes: 2_000,
                    key: false,
                    sent_to_ack_us: 80 * MS,
                    decode_us: 0,
                    was_sent: true
                },
                AckSample {
                    seq: 3,
                    bytes: 0,
                    key: false,
                    sent_to_ack_us: 0,
                    decode_us: 0,
                    was_sent: false
                },
                AckSample {
                    seq: 4,
                    bytes: 4_000,
                    key: false,
                    sent_to_ack_us: 70 * MS,
                    decode_us: 3_500,
                    was_sent: true
                },
            ]
        );
        let s = g.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (0, 0));
    }

    #[test]
    fn partial_ack_keeps_newer_frames() {
        let g = gate();
        for i in 0..3 {
            let seq = admit_seq(&g, i);
            g.on_sent(seq, 100, false, i);
        }
        assert_eq!(g.on_ack(2, 0, 10).len(), 2);
        let s = g.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (1, 100));
    }

    #[test]
    fn old_duplicate_and_future_acks_are_ignored() {
        let g = gate();
        assert!(g.on_ack(1, 0, 0).is_empty(), "nothing admitted yet");
        for i in 0..3 {
            admit_seq(&g, i);
        }
        assert_eq!(g.on_ack(1, 0, 10).len(), 1);
        assert!(g.on_ack(1, 0, 11).is_empty(), "duplicate");
        assert!(g.on_ack(0, 0, 12).is_empty(), "old");
        assert!(
            g.on_ack(4, 0, 13).is_empty(),
            "beyond the newest admitted frame"
        );
        assert!(g.on_ack(u32::MAX, 0, 14).is_empty(), "far future / bogus");
        assert_eq!(g.stats().inflight_frames, 2, "bogus ACKs release nothing");
        assert_eq!(g.on_ack(3, 0, 15).len(), 2);
    }

    #[test]
    fn duplicate_or_unknown_on_sent_is_ignored() {
        let g = gate();
        let a = admit_seq(&g, 0);
        g.on_sent(a, 1_000, false, 1);
        g.on_sent(a, 9_000, true, 2);
        g.on_sent(a + 5, 9_000, true, 2);
        let s = g.on_ack(a, 0, 11);
        assert_eq!(
            (s[0].bytes, s[0].key, s[0].sent_to_ack_us),
            (1_000, false, 10)
        );
        assert_eq!(g.stats().inflight_bytes, 0);
    }

    #[test]
    fn seq_wraps_skipping_zero() {
        let g = FlowGate::with_first_seq(FlowConfig::default(), u32::MAX - 1);
        let seqs: Vec<u32> = (0..4).map(|i| admit_seq(&g, i)).collect();
        assert_eq!(seqs, [u32::MAX - 1, u32::MAX, 1, 2]);
        assert!(g.on_ack(3, 0, 10).is_empty(), "future across the wrap");
        let released: Vec<u32> = g.on_ack(1, 0, 10).iter().map(|s| s.seq).collect();
        assert_eq!(released, [u32::MAX - 1, u32::MAX, 1]);
        assert!(g.on_ack(u32::MAX, 0, 11).is_empty(), "old across the wrap");
        assert_eq!(g.on_ack(2, 0, 12).len(), 1);
        assert_eq!(g.stats().last_seq, 2);
    }

    #[test]
    fn first_seq_zero_means_one() {
        let g = FlowGate::with_first_seq(FlowConfig::default(), 0);
        assert_eq!(admit_seq(&g, 0), 1);
    }

    #[test]
    fn rtt_min_sizes_the_window() {
        let g = gate();
        g.on_rtt_sample(30 * MS, 0);
        g.on_rtt_sample(10 * MS, 500 * MS);
        g.on_rtt_sample(20 * MS, 1_000 * MS);
        let s = g.stats();
        assert_eq!(s.rtt_min_us, 10 * MS);
        // ceil(60 * 0.05) + 1 = 4 frames; 1 MB/s * 0.05 s = 50_000 bytes.
        assert_eq!((s.window_frames, s.window_bytes), (4, 50_000));
        // The 10 ms sample expires 10 s after it was taken.
        g.on_rtt_sample(25 * MS, 10_501 * MS);
        assert_eq!(g.stats().rtt_min_us, 20 * MS);
    }

    #[test]
    fn window_follows_fps_and_target() {
        let g = gate();
        g.set_fps(120);
        g.set_target_bps(16_000_000);
        let s = g.stats();
        // ceil(120 * 0.09) + 1 = 12; 2 MB/s * 0.09 s.
        assert_eq!((s.window_frames, s.window_bytes), (12, 180_000));
        g.set_fps(1);
        assert_eq!(g.stats().window_frames, 2, "clamped to min_frames");
        g.set_fps(1_000);
        assert_eq!(g.stats().window_frames, 16, "clamped to max_frames");
        g.set_fps(0);
        assert_eq!(g.stats().window_frames, 2);
        assert!(g.admit(0).is_some(), "fps 0 must not divide by zero");
    }

    #[test]
    fn inconsistent_frame_clamps_do_not_panic() {
        let g = FlowGate::new(FlowConfig {
            min_frames: 10,
            max_frames: 3,
            ..FlowConfig::default()
        });
        assert_eq!(g.stats().window_frames, 10);
        let g = FlowGate::new(FlowConfig {
            initial_rtt_us: u64::MAX,
            ..FlowConfig::default()
        });
        assert_eq!(g.stats().window_frames, 16);
        assert!(g.admit(0).is_some());
    }

    #[test]
    fn oldest_inflight_age() {
        let g = gate();
        assert_eq!(g.oldest_inflight_age_us(0), None);
        let a = admit_seq(&g, 100);
        admit_seq(&g, 200);
        assert_eq!(g.oldest_inflight_age_us(1_600), Some(1_500));
        g.on_ack(a, 0, 1_700);
        assert_eq!(g.oldest_inflight_age_us(1_700), Some(1_500));
        assert_eq!(g.oldest_inflight_age_us(0), Some(0), "clock skew saturates");
    }

    #[test]
    fn reset_clears_window_but_keeps_counters() {
        let g = gate();
        // 5 ms RTT: W_frames = ceil(60 * 0.045) + 1 = 4.
        g.on_rtt_sample(5 * MS, 0);
        for i in 0..4 {
            let seq = admit_seq(&g, i);
            g.on_sent(seq, 1_000, false, i);
        }
        assert_eq!(g.admit(5), None);
        g.on_post_encode_drop(4);
        g.set_paused(true);
        g.reset();
        let s = g.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (0, 0));
        assert_eq!(
            (s.admitted_total, s.drop_window_total, s.drop_post_total),
            (4, 1, 1)
        );
        assert_eq!(s.rtt_min_us, 50_000, "RTT history forgotten");
        assert!(!s.paused);
        assert_eq!(g.oldest_inflight_age_us(10), None);
        // Seqs continue; old ACKs are now unknown; the receiver needs an IDR.
        assert!(g.on_ack(3, 0, 11).is_empty());
        assert_eq!(
            g.admit(12),
            Some(Admitted {
                seq: 5,
                force_keyframe: true
            })
        );
    }

    #[test]
    fn concurrent_use_keeps_accounting_consistent() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::mpsc;

        let g = FlowGate::new(FlowConfig {
            max_frames: 64,
            ..FlowConfig::default()
        });
        let clock = AtomicU64::new(0);
        let (sent_tx, sent_rx) = mpsc::channel::<u32>();
        std::thread::scope(|s| {
            // Capture/encoder thread: admit, then either send or cancel.
            s.spawn(|| {
                for i in 0..20_000u32 {
                    let now = clock.fetch_add(10, Ordering::Relaxed);
                    if let Some(a) = g.admit(now) {
                        if i.is_multiple_of(7) {
                            g.cancel(a.seq);
                        } else {
                            g.on_sent(a.seq, 100, false, now);
                            sent_tx.send(a.seq).unwrap();
                        }
                    }
                }
                drop(sent_tx);
            });
            // Network thread: ACK everything that was sent, plus PINGs and stats.
            s.spawn(|| {
                for seq in sent_rx {
                    let now = clock.fetch_add(10, Ordering::Relaxed);
                    g.on_ack(seq, 0, now);
                    if seq.is_multiple_of(100) {
                        g.on_rtt_sample(1_000, now);
                        let _ = g.stats();
                    }
                }
            });
        });
        let s = g.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (0, 0));
        assert!(s.admitted_total > 0);
        assert_eq!(s.admitted_total + s.drop_window_total, 20_000);
    }
}
