//! Delay-based bitrate controller (ticks every 250 ms).
//!
//! ```text
//! serialization_i = bytes_i * 8 / max(delivery_rate, 1 Mbps)
//! q_i = sent_to_ack_i - rtt_min - serialization_i - decode_i      (sent frames only)
//! q   = median(q_i over last 500 ms, excluding keyframes)
//! delivery_rate = bits ACKed in the last 1 s (per second)
//! app_limited   = encoded bitrate(last 1 s) < 0.7 * target        (static screen: don't ramp)
//! window_drop_ratio = window drops / captured frames over the last 1 s
//!
//! if q > 150 ms:                         target = max(min, 0.7 * delivery_rate); hold 2 s
//! elif (q > 50 ms for 2 ticks) or window_drop_ratio > 0.10:
//!                                        target = max(min, min(0.85 * target, 0.95 * delivery_rate)); hold 2 s
//! elif past hold and q < 15 ms and !app_limited and no window drops for 1 s:
//!     every 500 ms:                      target = min(max, 1.06 * target + 200 kbps)
//! startup (first 3 s while q < 15 ms):   target *= 1.25 per tick (capped at max)
//! report a change only if |Δ| > 5 %.
//! ```
//! When `adaptive == false` the target stays at `start_bps` (clamped to [min,max]).
//!
//! ## Refinements of the law above
//! Each one fixes a failure of the literal law seen in the closed-loop simulation
//! (`tests/sim.rs`; `sweep` prints the scenario matrix used for tuning):
//!
//! - **Fresh evidence only after a cut.** `q` and the drop ratio only use frames *sent* /
//!   *captured* after the most recent cut. Otherwise the same stale samples (frames queued
//!   before the cut, drops still inside the 1 s window) cut again on every following tick and
//!   the target collapses (seen: 16.8 → 8.8 Mbps within 1 s of one keyframe).
//! - **Drop-ratio cuts need a queue** (`q >= 15 ms`). A keyframe larger than `W_bytes` makes the
//!   gate drop the frames behind it until it is ACKed: 10–25 % of a second's frames on an idle
//!   link. Without the guard every keyframe cuts the target (and the very first one kills the
//!   startup ramp).
//! - **Standing-queue cut:** `q >= 5 ms` together with window drops on 4 consecutive ticks is a
//!   mild cut. Slightly above the link rate the window keeps the queue short by dropping
//!   frames (q ≈ 3–13 ms, drop ratio 5–10 %), so neither cut fires and no increase is allowed:
//!   the target would sit above the link rate forever, dropping ~7 % of the frames, and the
//!   in-band PINGs would see a standing queue (`rtt_min`, and with it the window, creeps up).
//!   Below the link rate `q` is ~0 (the serialization estimate even biases it negative), so
//!   burst-induced drops do not trigger this.
//! - **Increases need 2 s without window drops** (instead of 1 s). With realistic frame-size
//!   jitter the window drops a few percent of the frames already at ~0.9 x the link rate; a
//!   drop-free second still happens often enough there that the target drifted to a mean of
//!   1.0–1.05 x the link rate. With 2 s it settles at 0.8–0.98 x. The cost: very bursty content
//!   (frame sizes ±90 %) probes upwards more slowly.
//! - **Cuts never raise the target** (e.g. when a burst of ACKs makes the delivery rate exceed
//!   it).
//! - **The startup ramp also requires `!app_limited`** (a static screen never ramps) and ignores
//!   window drops (the first keyframe always causes some).
//! - Until the first full second has elapsed, rates are averaged over the elapsed time.
//! - In fixed mode every change of the target (via `set_config`) is reported, not only > 5 %.

use std::collections::VecDeque;

use crate::AckSample;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BitrateConfig {
    pub start_bps: u32,
    pub min_bps: u32,
    pub max_bps: u32,
    pub adaptive: bool,
}

/// Why the controller changed (or kept) the target — for logs/HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateDecision {
    Hold,
    Startup,
    Increase,
    DecreaseMild,
    DecreaseHard,
}

const MS: u64 = 1_000;
const SEC: u64 = 1_000_000;

const TICK_US: u64 = 250 * MS;
/// Window for delivery rate, encoded rate and drop ratio.
const RATE_WINDOW_US: u64 = SEC;
/// Window for the queueing-delay median.
const Q_WINDOW_US: u64 = 500 * MS;
const STARTUP_US: u64 = 3 * SEC;
const HOLD_US: u64 = 2 * SEC;
const INCREASE_INTERVAL_US: u64 = 500 * MS;
/// Increases need this long without a window drop.
const DROP_FREE_US: u64 = 2 * SEC;

const Q_HARD_US: i64 = 150_000;
const Q_MILD_US: i64 = 50_000;
const Q_LOW_US: i64 = 15_000;
/// Consecutive ticks with `q > Q_MILD_US` before a mild cut.
const HIGH_Q_TICKS: u32 = 2;
/// A standing queue: `q >= Q_STANDING_US` with window drops for this many consecutive ticks.
const Q_STANDING_US: i64 = 5_000;
const STANDING_QUEUE_TICKS: u32 = 4;

const DROP_RATIO_CUT: f64 = 0.10;
const APP_LIMITED_RATIO: f64 = 0.7;
const HARD_CUT_OF_DELIVERY: f64 = 0.7;
const MILD_CUT_OF_TARGET: f64 = 0.85;
const MILD_CUT_OF_DELIVERY: f64 = 0.95;
const STARTUP_GAIN: f64 = 1.25;
const INCREASE_GAIN: f64 = 1.06;
const INCREASE_STEP_BPS: f64 = 200_000.0;
/// Serialization is estimated at no less than this rate (avoids absurd estimates when the
/// delivery rate is tiny, e.g. a static screen).
const MIN_SERIALIZATION_BPS: u64 = 1_000_000;
/// Report a new target only when it differs from the last reported one by more than 1/20.
const REPORT_THRESHOLD_DIV: u64 = 20;
/// Safety cap per event history (a 1 s window at 120 fps needs ~120 entries).
const MAX_EVENTS: usize = 4096;

#[derive(Debug, Clone, Copy)]
struct AckRecord {
    at_us: u64,
    bytes: usize,
    sent_to_ack_us: u64,
    decode_us: u32,
    key: bool,
}

impl AckRecord {
    fn sent_at_us(&self) -> u64 {
        self.at_us.saturating_sub(self.sent_to_ack_us)
    }
}

/// What one tick measured.
#[derive(Debug, Clone, Copy, Default)]
struct Measurement {
    q_us: Option<i64>,
    delivery_bps: u64,
    encoded_bps: u64,
    /// Captured frames / window drops in the last second, since the last cut.
    captured: usize,
    drops: usize,
}

impl Measurement {
    fn drop_ratio(&self) -> f64 {
        if self.captured == 0 {
            0.0
        } else {
            self.drops as f64 / self.captured as f64
        }
    }
}

#[derive(Debug, Clone)]
pub struct BitrateController {
    cfg: BitrateConfig,
    target_bps: u64,
    reported_bps: u32,
    started_at_us: u64,
    in_startup: bool,
    /// Time of the last cut: older samples are not evidence for further decisions.
    fresh_since_us: u64,
    /// No increases before this time (set by every cut).
    hold_until_us: u64,
    last_increase_us: u64,
    last_window_drop_us: Option<u64>,
    high_q_ticks: u32,
    standing_queue_ticks: u32,
    decision: BitrateDecision,
    last: Measurement,
    acks: VecDeque<AckRecord>,
    /// `(time, bytes)` of encoded frames.
    encoded: VecDeque<(u64, usize)>,
    /// `(time, dropped_by_window)` of captured frames.
    captured: VecDeque<(u64, bool)>,
}

impl BitrateController {
    pub fn new(cfg: BitrateConfig, now_us: u64) -> Self {
        let target_bps = clamp_bps(&cfg, u64::from(cfg.start_bps));
        BitrateController {
            cfg,
            target_bps,
            reported_bps: target_bps as u32,
            started_at_us: now_us,
            in_startup: cfg.adaptive,
            fresh_since_us: now_us,
            hold_until_us: now_us,
            last_increase_us: now_us,
            last_window_drop_us: None,
            high_q_ticks: 0,
            standing_queue_ticks: 0,
            decision: BitrateDecision::Hold,
            last: Measurement::default(),
            acks: VecDeque::new(),
            encoded: VecDeque::new(),
            captured: VecDeque::new(),
        }
    }

    /// Change limits / mode (viewer `set`). Target is re-clamped.
    ///
    /// In fixed mode (`adaptive == false`) the target becomes the new `start_bps` (clamped).
    /// The change is reported by the next [`BitrateController::tick`].
    pub fn set_config(&mut self, cfg: BitrateConfig) {
        self.cfg = cfg;
        let base = if cfg.adaptive {
            self.target_bps
        } else {
            u64::from(cfg.start_bps)
        };
        self.target_bps = clamp_bps(&cfg, base);
        if !cfg.adaptive {
            self.in_startup = false;
        }
    }

    /// Feed ACK samples from [`crate::FlowGate::on_ack`].
    pub fn on_ack_samples(&mut self, samples: &[AckSample], now_us: u64) {
        for s in samples.iter().filter(|s| s.was_sent) {
            push_capped(
                &mut self.acks,
                AckRecord {
                    at_us: now_us,
                    bytes: s.bytes,
                    sent_to_ack_us: s.sent_to_ack_us,
                    decode_us: s.decode_us,
                    key: s.key,
                },
            );
        }
        self.prune(now_us);
    }

    /// An encoded frame of `bytes` was produced (app-limited detection).
    pub fn on_encoded(&mut self, bytes: usize, now_us: u64) {
        push_capped(&mut self.encoded, (now_us, bytes));
        self.prune(now_us);
    }

    /// A frame was captured (`dropped_by_window` = gate returned None).
    pub fn on_captured(&mut self, dropped_by_window: bool, now_us: u64) {
        if dropped_by_window {
            self.last_window_drop_us = Some(now_us);
        }
        push_capped(&mut self.captured, (now_us, dropped_by_window));
        self.prune(now_us);
    }

    /// Run the control law; call every 250 ms. Returns `Some(new_target_bps)` only when the
    /// target changed by more than 5 % since the last reported value.
    pub fn tick(&mut self, now_us: u64, rtt_min_us: u64) -> Option<u32> {
        self.prune(now_us);
        self.last = self.measure(now_us, rtt_min_us);
        if self.cfg.adaptive {
            self.decision = self.decide(now_us);
        } else {
            self.target_bps = clamp_bps(&self.cfg, u64::from(self.cfg.start_bps));
            self.decision = BitrateDecision::Hold;
        }
        self.report()
    }

    pub fn target_bps(&self) -> u32 {
        self.target_bps as u32
    }

    pub fn last_decision(&self) -> BitrateDecision {
        self.decision
    }

    /// Current queueing-delay estimate `q` in microseconds (for stats).
    pub fn queue_delay_us(&self) -> u64 {
        self.last.q_us.map_or(0, |q| q.max(0) as u64)
    }

    /// Delivery rate over the last second, bits/s.
    pub fn delivery_bps(&self) -> u64 {
        self.last.delivery_bps
    }

    /// Encoded bitrate over the last second, bits/s.
    pub fn encoded_bps(&self) -> u64 {
        self.last.encoded_bps
    }

    fn prune(&mut self, now_us: u64) {
        let horizon = now_us.saturating_sub(RATE_WINDOW_US);
        while self.acks.front().is_some_and(|a| a.at_us < horizon) {
            self.acks.pop_front();
        }
        while self.encoded.front().is_some_and(|&(t, _)| t < horizon) {
            self.encoded.pop_front();
        }
        while self.captured.front().is_some_and(|&(t, _)| t < horizon) {
            self.captured.pop_front();
        }
    }

    /// Evaluate the (pruned) histories at `now_us`.
    fn measure(&self, now_us: u64, rtt_min_us: u64) -> Measurement {
        // Until a full window has elapsed, rates are averaged over the elapsed time only.
        let span_us = now_us
            .saturating_sub(self.started_at_us)
            .clamp(TICK_US, RATE_WINDOW_US);
        let rate = |bytes: u64| bytes.saturating_mul(8 * SEC) / span_us;
        let delivery_bps = rate(self.acks.iter().map(|a| a.bytes as u64).sum());
        let encoded_bps = rate(self.encoded.iter().map(|&(_, b)| b as u64).sum());

        let ser_bps = delivery_bps.max(MIN_SERIALIZATION_BPS);
        let q_horizon = now_us.saturating_sub(Q_WINDOW_US);
        let mut qs: Vec<i64> = self
            .acks
            .iter()
            .filter(|a| !a.key && a.at_us >= q_horizon && a.sent_at_us() >= self.fresh_since_us)
            .map(|a| {
                let ser_us = (a.bytes as u64).saturating_mul(8 * SEC) / ser_bps;
                signed(a.sent_to_ack_us)
                    .saturating_sub(signed(rtt_min_us))
                    .saturating_sub(signed(ser_us))
                    .saturating_sub(i64::from(a.decode_us))
            })
            .collect();
        let q_us = if qs.is_empty() {
            None
        } else {
            let mid = qs.len() / 2;
            Some(*qs.select_nth_unstable(mid).1)
        };

        let fresh = self
            .captured
            .iter()
            .filter(|&&(t, _)| t >= self.fresh_since_us);
        let (captured, drops) = fresh.fold((0, 0), |(c, d), &(_, dropped)| {
            (c + 1, d + usize::from(dropped))
        });
        Measurement {
            q_us,
            delivery_bps,
            encoded_bps,
            captured,
            drops,
        }
    }

    fn decide(&mut self, now_us: u64) -> BitrateDecision {
        let m = self.last;
        if now_us.saturating_sub(self.started_at_us) >= STARTUP_US {
            self.in_startup = false;
        }
        // Tick counters only move on ticks that have fresh samples.
        if let Some(q) = m.q_us {
            self.high_q_ticks = if q > Q_MILD_US {
                self.high_q_ticks + 1
            } else {
                0
            };
            self.standing_queue_ticks = if q >= Q_STANDING_US && m.drops > 0 {
                self.standing_queue_ticks + 1
            } else {
                0
            };
        }
        let q_low = m.q_us.is_some_and(|q| q < Q_LOW_US);
        let queueing = m.q_us.is_some_and(|q| q >= Q_LOW_US);
        let app_limited = (m.encoded_bps as f64) < APP_LIMITED_RATIO * self.target_bps as f64;
        let target = self.target_bps as f64;
        let delivery = m.delivery_bps as f64;

        if m.q_us.is_some_and(|q| q > Q_HARD_US) {
            self.cut(now_us, HARD_CUT_OF_DELIVERY * delivery);
            BitrateDecision::DecreaseHard
        } else if self.high_q_ticks >= HIGH_Q_TICKS
            || (queueing && m.drop_ratio() > DROP_RATIO_CUT)
            || self.standing_queue_ticks >= STANDING_QUEUE_TICKS
        {
            self.cut(
                now_us,
                (MILD_CUT_OF_TARGET * target).min(MILD_CUT_OF_DELIVERY * delivery),
            );
            BitrateDecision::DecreaseMild
        } else if self.in_startup && q_low && !app_limited {
            self.set_target(target * STARTUP_GAIN);
            BitrateDecision::Startup
        } else if now_us >= self.hold_until_us
            && q_low
            && !app_limited
            && self
                .last_window_drop_us
                .is_none_or(|t| now_us.saturating_sub(t) >= DROP_FREE_US)
            && now_us.saturating_sub(self.last_increase_us) >= INCREASE_INTERVAL_US
        {
            self.set_target(target * INCREASE_GAIN + INCREASE_STEP_BPS);
            self.last_increase_us = now_us;
            BitrateDecision::Increase
        } else {
            BitrateDecision::Hold
        }
    }

    /// Lower the target to `new_bps` (never raises it) and start a hold period.
    fn cut(&mut self, now_us: u64, new_bps: f64) {
        self.set_target(new_bps.min(self.target_bps as f64));
        self.fresh_since_us = now_us;
        self.hold_until_us = now_us.saturating_add(HOLD_US);
        self.high_q_ticks = 0;
        self.standing_queue_ticks = 0;
        self.in_startup = false;
    }

    fn set_target(&mut self, bps: f64) {
        self.target_bps = clamp_bps(&self.cfg, bps as u64);
    }

    fn report(&mut self) -> Option<u32> {
        let target = self.target_bps as u32;
        let reported = self.reported_bps;
        let diff = u64::from(target.abs_diff(reported));
        let changed = if self.cfg.adaptive {
            diff * REPORT_THRESHOLD_DIV > u64::from(reported)
                || !bounds(&self.cfg).contains(&u64::from(reported))
        } else {
            diff != 0
        };
        if changed {
            self.reported_bps = target;
            Some(target)
        } else {
            None
        }
    }
}

fn signed(us: u64) -> i64 {
    i64::try_from(us).unwrap_or(i64::MAX)
}

/// `[min, max]`; `min` wins if the config is inconsistent.
fn bounds(cfg: &BitrateConfig) -> std::ops::RangeInclusive<u64> {
    let lo = u64::from(cfg.min_bps);
    lo..=u64::from(cfg.max_bps).max(lo)
}

fn clamp_bps(cfg: &BitrateConfig, bps: u64) -> u64 {
    let range = bounds(cfg);
    bps.clamp(*range.start(), *range.end())
}

fn push_capped<T>(q: &mut VecDeque<T>, item: T) {
    if q.len() >= MAX_EVENTS {
        q.pop_front();
    }
    q.push_back(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: u32 = 1_000_000;
    const RTT: u64 = 40 * MS;
    const FRAME_US: u64 = SEC / 60;

    fn cfg(start: u32, min: u32, max: u32) -> BitrateConfig {
        BitrateConfig {
            start_bps: start,
            min_bps: min,
            max_bps: max,
            adaptive: true,
        }
    }

    /// Steady traffic for the controller: 60 fps, every frame encoded at `bytes` and ACKed
    /// `sent_to_ack_us` later (except every `drop_every`-th frame, dropped by the window).
    #[derive(Clone, Copy)]
    struct Traffic {
        bytes: usize,
        sent_to_ack_us: u64,
        decode_us: u32,
        drop_every: Option<u64>,
    }

    impl Traffic {
        /// Full-size frames for `bps` with an extra queueing delay of `q_us`.
        fn at(bps: u32, q_us: u64) -> Self {
            let bytes = (u64::from(bps) / 8 / 60) as usize;
            // sent_to_ack = rtt + serialization at the delivery rate + decode + q.
            let ser_us = bytes as u64 * 8 * SEC / u64::from(bps).max(MIN_SERIALIZATION_BPS);
            Traffic {
                bytes,
                sent_to_ack_us: RTT + ser_us + 3 * MS + q_us,
                decode_us: 3_000,
                drop_every: None,
            }
        }

        fn dropping(mut self, every: u64) -> Self {
            self.drop_every = Some(every);
            self
        }
    }

    struct Harness {
        c: BitrateController,
        now: u64,
        frame: u64,
    }

    impl Harness {
        fn new(cfg: BitrateConfig) -> Self {
            Harness {
                c: BitrateController::new(cfg, 0),
                now: 0,
                frame: 0,
            }
        }

        /// Feed 250 ms of `traffic`, then tick. Returns the tick result.
        fn tick(&mut self, traffic: Traffic) -> Option<u32> {
            let end = self.now + TICK_US;
            while self.now + FRAME_US <= end {
                self.now += FRAME_US;
                self.frame += 1;
                let dropped = traffic
                    .drop_every
                    .is_some_and(|n| self.frame.is_multiple_of(n));
                self.c.on_captured(dropped, self.now);
                if !dropped {
                    self.c.on_encoded(traffic.bytes, self.now);
                    let sample = AckSample {
                        seq: self.frame as u32,
                        bytes: traffic.bytes,
                        key: false,
                        sent_to_ack_us: traffic.sent_to_ack_us,
                        decode_us: traffic.decode_us,
                        was_sent: true,
                    };
                    self.c.on_ack_samples(&[sample], self.now);
                }
            }
            self.now = end;
            self.c.tick(self.now, RTT)
        }

        fn ticks(&mut self, n: usize, traffic: Traffic) -> Vec<(Option<u32>, BitrateDecision)> {
            (0..n)
                .map(|_| (self.tick(traffic), self.c.last_decision()))
                .collect()
        }

        /// Run past the startup phase with full-rate traffic whose queue (20 ms) neither ramps
        /// nor cuts. Its delay samples linger in the 500 ms `q` window for one more tick.
        fn skip_startup(&mut self) {
            let hold_zone = Traffic::at(self.c.target_bps(), 20 * MS);
            for (_, decision) in self.ticks((STARTUP_US / TICK_US) as usize, hold_zone) {
                assert_eq!(decision, BitrateDecision::Hold);
            }
            assert!(!self.c.in_startup);
        }
    }

    #[test]
    fn start_is_clamped() {
        let c = BitrateController::new(cfg(50 * MBPS, MBPS, 20 * MBPS), 0);
        assert_eq!(c.target_bps(), 20 * MBPS);
        let c = BitrateController::new(cfg(100, MBPS, 20 * MBPS), 0);
        assert_eq!(c.target_bps(), MBPS);
        assert_eq!(c.last_decision(), BitrateDecision::Hold);
        // min > max must not panic: min wins, and is not re-reported on every tick.
        let mut c = BitrateController::new(cfg(5 * MBPS, 8 * MBPS, 2 * MBPS), 0);
        assert_eq!(c.target_bps(), 8 * MBPS);
        assert_eq!(c.tick(TICK_US, RTT), None);
        assert_eq!(c.tick(2 * TICK_US, RTT), None);
    }

    #[test]
    fn startup_ramps_by_a_quarter_per_tick() {
        let mut h = Harness::new(cfg(2 * MBPS, MBPS, 100 * MBPS));
        let mut target = 2 * MBPS;
        for _ in 0..4 {
            let reported = h.tick(Traffic::at(target, 0));
            assert_eq!(h.c.last_decision(), BitrateDecision::Startup);
            target = (f64::from(target) * STARTUP_GAIN) as u32;
            assert_eq!(h.c.target_bps(), target);
            assert_eq!(reported, Some(target));
        }
    }

    #[test]
    fn startup_ends_after_three_seconds() {
        let mut h = Harness::new(cfg(MBPS, MBPS, 1_000 * MBPS));
        let mut decisions = Vec::new();
        for _ in 0..16 {
            h.tick(Traffic::at(h.c.target_bps(), 0));
            decisions.push(h.c.last_decision());
        }
        let startup_ticks = decisions
            .iter()
            .filter(|d| **d == BitrateDecision::Startup)
            .count();
        assert_eq!(startup_ticks as u64, STARTUP_US / TICK_US - 1);
        assert!(
            decisions[12..]
                .iter()
                .all(|d| *d != BitrateDecision::Startup)
        );
    }

    #[test]
    fn startup_needs_queue_to_be_low() {
        let mut h = Harness::new(cfg(4 * MBPS, MBPS, 100 * MBPS));
        assert_eq!(h.tick(Traffic::at(4 * MBPS, 30 * MS)), None);
        assert_eq!(h.c.last_decision(), BitrateDecision::Hold);
        assert_eq!(h.c.target_bps(), 4 * MBPS);
    }

    #[test]
    fn app_limited_source_never_ramps() {
        let mut h = Harness::new(cfg(4 * MBPS, MBPS, 100 * MBPS));
        // A static screen: 500-byte frames (240 kbps) against a 4 Mbps target.
        let tiny = Traffic {
            bytes: 500,
            ..Traffic::at(4 * MBPS, 0)
        };
        for (reported, decision) in h.ticks(40, tiny) {
            assert_eq!(reported, None);
            assert_eq!(decision, BitrateDecision::Hold);
        }
        assert_eq!(h.c.target_bps(), 4 * MBPS);
        assert!(h.c.encoded_bps() < 300_000);
    }

    #[test]
    fn increases_every_500_ms_after_startup() {
        let mut h = Harness::new(cfg(4 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let mut increases = Vec::new();
        for _ in 0..9 {
            let before = h.c.target_bps();
            h.tick(Traffic::at(before, 0));
            if h.c.last_decision() == BitrateDecision::Increase {
                let expected = (f64::from(before) * INCREASE_GAIN + INCREASE_STEP_BPS) as u32;
                assert_eq!(h.c.target_bps(), expected);
                increases.push(h.now);
            }
        }
        assert_eq!(increases.len(), 4);
        assert!(
            increases
                .windows(2)
                .all(|w| w[1] - w[0] == INCREASE_INTERVAL_US)
        );
    }

    #[test]
    fn increase_is_capped_at_max() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 10_500_000));
        h.skip_startup();
        h.ticks(4, Traffic::at(10 * MBPS, 0));
        assert_eq!(h.c.target_bps(), 10_500_000);
    }

    #[test]
    fn hard_cut_when_queue_exceeds_150_ms() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        h.ticks(4, Traffic::at(10 * MBPS, 0));
        let before = h.c.target_bps();
        let reported = h.tick(Traffic::at(before, 200 * MS));
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseHard);
        let expected = (HARD_CUT_OF_DELIVERY * h.c.delivery_bps() as f64) as u32;
        assert_eq!(h.c.target_bps(), expected);
        assert_eq!(reported, Some(expected));
        assert!(h.c.queue_delay_us() > 150_000);
    }

    #[test]
    fn hard_cut_respects_min() {
        let mut h = Harness::new(cfg(2 * MBPS, 1_900_000, 100 * MBPS));
        h.tick(Traffic::at(2 * MBPS, 300 * MS));
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseHard);
        assert_eq!(h.c.target_bps(), 1_900_000);
    }

    #[test]
    fn mild_cut_after_two_ticks_above_50_ms() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let t = h.c.target_bps();
        let congested = Traffic::at(t, 70 * MS);
        assert_eq!(h.tick(congested), None);
        assert_eq!(h.c.last_decision(), BitrateDecision::Hold);
        h.tick(congested);
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseMild);
        let expected = (MILD_CUT_OF_TARGET * f64::from(t))
            .min(MILD_CUT_OF_DELIVERY * h.c.delivery_bps() as f64) as u32;
        assert_eq!(h.c.target_bps(), expected);
    }

    #[test]
    fn drop_ratio_cut_needs_a_queue() {
        // 20 % window drops on an idle link (keyframe bursts): no cut.
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let t = h.c.target_bps();
        for (_, decision) in h.ticks(8, Traffic::at(t, 0).dropping(5)) {
            assert_eq!(decision, BitrateDecision::Hold);
        }
        // The same drops with a 20 ms queue: cut.
        h.tick(Traffic::at(t, 20 * MS).dropping(5));
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseMild);
    }

    #[test]
    fn standing_queue_cut() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let t = h.c.target_bps();
        // An 8 ms queue and a single drop (it stays in the 1 s window for 4 ticks): below every
        // other threshold.
        h.c.on_captured(true, h.now + 1);
        let standing = Traffic::at(t, 8 * MS);
        let decisions: Vec<_> = h.ticks(4, standing).into_iter().map(|(_, d)| d).collect();
        assert_eq!(
            decisions,
            [
                BitrateDecision::Hold,
                BitrateDecision::Hold,
                BitrateDecision::Hold,
                BitrateDecision::DecreaseMild
            ]
        );
    }

    #[test]
    fn no_increase_within_two_seconds_of_a_drop() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let t = h.c.target_bps();
        let drop_at = h.now + 1;
        h.c.on_captured(true, drop_at);
        let first_increase = loop {
            h.tick(Traffic::at(t, 0));
            if h.c.last_decision() == BitrateDecision::Increase {
                break h.now;
            }
            assert!(
                h.now < drop_at + 2 * DROP_FREE_US,
                "increases never resumed"
            );
        };
        assert!(first_increase - drop_at >= DROP_FREE_US);
        assert!(first_increase - drop_at < DROP_FREE_US + TICK_US);
    }

    #[test]
    fn hold_after_cut_blocks_increases() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        h.tick(Traffic::at(h.c.target_bps(), 200 * MS));
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseHard);
        let cut_at = h.now;
        let t = h.c.target_bps();
        loop {
            h.tick(Traffic::at(t, 0));
            if h.c.last_decision() == BitrateDecision::Increase {
                break;
            }
            assert!(h.now < cut_at + 4 * SEC, "never increased again");
        }
        assert!(h.now >= cut_at + HOLD_US);
    }

    #[test]
    fn stale_samples_do_not_cut_twice() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.skip_startup();
        let t = h.c.target_bps();
        h.tick(Traffic::at(t, 200 * MS));
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseHard);
        let after_cut = h.c.target_bps();
        // Frames sent before the cut keep arriving with a huge delay: no further cut.
        let now = h.now + 10 * MS;
        let stale = AckSample {
            seq: 0,
            bytes: 20_000,
            key: false,
            sent_to_ack_us: 300 * MS,
            decode_us: 3_000,
            was_sent: true,
        };
        h.c.on_ack_samples(&[stale; 10], now);
        assert_eq!(h.c.tick(now + TICK_US, RTT), None);
        assert_eq!(h.c.last_decision(), BitrateDecision::Hold);
        assert_eq!(h.c.target_bps(), after_cut);
        assert_eq!(h.c.queue_delay_us(), 0, "no fresh samples");
    }

    #[test]
    fn cut_never_raises_the_target() {
        let mut h = Harness::new(cfg(2 * MBPS, MBPS, 100 * MBPS));
        // Delivery (20 Mbps worth of ACKs) far above the 2 Mbps target, with a big queue.
        h.tick(Traffic {
            bytes: 40_000,
            ..Traffic::at(2 * MBPS, 300 * MS)
        });
        assert_eq!(h.c.last_decision(), BitrateDecision::DecreaseHard);
        assert!(h.c.target_bps() <= 2 * MBPS);
    }

    #[test]
    fn keyframes_and_unsent_frames_are_not_delay_samples() {
        let mut c = BitrateController::new(cfg(10 * MBPS, MBPS, 100 * MBPS), 0);
        let key = AckSample {
            seq: 1,
            bytes: 200_000,
            key: true,
            sent_to_ack_us: SEC,
            decode_us: 0,
            was_sent: true,
        };
        let unsent = AckSample {
            seq: 2,
            bytes: 0,
            key: false,
            sent_to_ack_us: 0,
            decode_us: 0,
            was_sent: false,
        };
        c.on_ack_samples(&[key, unsent], 100 * MS);
        assert_eq!(c.tick(TICK_US, RTT), None);
        assert_eq!(c.last_decision(), BitrateDecision::Hold);
        assert_eq!(c.queue_delay_us(), 0);
        // The keyframe still counts as delivered data.
        assert_eq!(c.delivery_bps(), 200_000 * 8 * 4);
    }

    #[test]
    fn queue_delay_estimate() {
        let mut c = BitrateController::new(cfg(8 * MBPS, MBPS, 100 * MBPS), 0);
        // 10 frames of 10 kB over the first 250 ms => delivery 3.2 Mbps; serialization of one
        // frame at that rate = 25 ms. q = 100 - 40 - 25 - 5 = 30 ms.
        for i in 0..10 {
            let s = AckSample {
                seq: i,
                bytes: 10_000,
                key: false,
                sent_to_ack_us: 100 * MS,
                decode_us: 5_000,
                was_sent: true,
            };
            c.on_ack_samples(&[s], 150 * MS + u64::from(i) * MS);
        }
        c.tick(TICK_US, RTT);
        assert_eq!(c.delivery_bps(), 3_200_000);
        assert_eq!(c.queue_delay_us(), 30 * MS);
    }

    #[test]
    fn rates_use_elapsed_time_in_the_first_second() {
        let mut c = BitrateController::new(cfg(8 * MBPS, MBPS, 100 * MBPS), 0);
        c.on_encoded(25_000, 100 * MS);
        c.tick(500 * MS, RTT);
        assert_eq!(c.encoded_bps(), 400_000);
        // Later the window is the last full second.
        c.on_encoded(25_000, 1_400 * MS);
        c.tick(2 * SEC, RTT);
        assert_eq!(c.encoded_bps(), 200_000);
    }

    #[test]
    fn history_is_pruned_to_one_second() {
        let mut c = BitrateController::new(cfg(8 * MBPS, MBPS, 100 * MBPS), 0);
        for i in 0..10_000u64 {
            c.on_captured(false, i * MS);
            c.on_encoded(100, i * MS);
        }
        assert!(c.captured.len() <= 1_001);
        assert!(c.encoded.len() <= 1_001);
        // A caller whose clock stands still is bounded too.
        for _ in 0..10_000 {
            c.on_captured(true, 10 * SEC);
        }
        assert_eq!(c.captured.len(), MAX_EVENTS);
    }

    #[test]
    fn reports_only_changes_above_five_percent() {
        let mut c = BitrateController::new(cfg(10 * MBPS, MBPS, 100 * MBPS), 0);
        c.target_bps = 10_400_000;
        assert_eq!(c.report(), None);
        c.target_bps = 10_600_000;
        assert_eq!(c.report(), Some(10_600_000));
        c.target_bps = 10_100_000;
        assert_eq!(c.report(), None, "4.7 % below the last reported value");
        c.target_bps = 9_900_000;
        assert_eq!(c.report(), Some(9_900_000));
    }

    #[test]
    fn set_config_reclamps_and_reports() {
        let mut h = Harness::new(cfg(10 * MBPS, MBPS, 100 * MBPS));
        h.c.set_config(cfg(10 * MBPS, MBPS, 6 * MBPS));
        assert_eq!(h.c.target_bps(), 6 * MBPS);
        // Reported on the next tick even though the traffic would ramp: capped at max.
        assert_eq!(h.tick(Traffic::at(6 * MBPS, 0)), Some(6 * MBPS));
        h.c.set_config(cfg(10 * MBPS, 7 * MBPS, 50 * MBPS));
        assert_eq!(h.c.target_bps(), 7 * MBPS);
        // A tiny clamp (< 5 %) is still reported because the old value is out of range.
        h.c.set_config(cfg(10 * MBPS, 7_200_000, 50 * MBPS));
        assert_eq!(h.c.tick(h.now + TICK_US, RTT), Some(7_200_000));
    }

    #[test]
    fn fixed_mode_keeps_start() {
        let mut h = Harness::new(BitrateConfig {
            adaptive: false,
            ..cfg(8 * MBPS, MBPS, 20 * MBPS)
        });
        for traffic in [
            Traffic::at(8 * MBPS, 0),
            Traffic::at(8 * MBPS, 300 * MS).dropping(2),
        ] {
            for (reported, decision) in h.ticks(20, traffic) {
                assert_eq!(reported, None);
                assert_eq!(decision, BitrateDecision::Hold);
            }
        }
        assert_eq!(h.c.target_bps(), 8 * MBPS);
        // Measurements still run (for stats).
        assert!(h.c.queue_delay_us() > 250_000);
    }

    #[test]
    fn fixed_mode_reports_every_change() {
        let fixed = |start| BitrateConfig {
            adaptive: false,
            ..cfg(start, MBPS, 20 * MBPS)
        };
        let mut c = BitrateController::new(fixed(8 * MBPS), 0);
        c.set_config(fixed(8_100_000));
        assert_eq!(c.target_bps(), 8_100_000);
        assert_eq!(c.tick(TICK_US, RTT), Some(8_100_000));
        c.set_config(fixed(50 * MBPS));
        assert_eq!(c.tick(2 * TICK_US, RTT), Some(20 * MBPS));
        assert_eq!(c.tick(3 * TICK_US, RTT), None);
    }

    #[test]
    fn switching_modes() {
        let mut h = Harness::new(cfg(8 * MBPS, MBPS, 20 * MBPS));
        h.ticks(2, Traffic::at(8 * MBPS, 0));
        assert!(h.c.target_bps() > 8 * MBPS);
        // To fixed: back to start.
        h.c.set_config(BitrateConfig {
            adaptive: false,
            ..cfg(8 * MBPS, MBPS, 20 * MBPS)
        });
        assert_eq!(h.c.target_bps(), 8 * MBPS);
        // Back to adaptive: continues from the current target, no second startup.
        h.c.set_config(cfg(8 * MBPS, MBPS, 20 * MBPS));
        h.tick(Traffic::at(8 * MBPS, 0));
        assert_ne!(h.c.last_decision(), BitrateDecision::Startup);
    }
}
