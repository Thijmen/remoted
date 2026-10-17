//! Deterministic discrete-event simulation of the whole flow-control loop:
//!
//! ```text
//! capture 60 fps -> FlowGate::admit -> encoder (4 ms) -> bottleneck link (bandwidth B(t),
//! deep FIFO) -> one-way delay D -> viewer decoder (3 ms) -> cumulative ACK -> delay D -> agent
//! ```
//!
//! The agent PINGs every 500 ms over the same FIFO (so RTT samples include queueing, as they do
//! on the real TCP stream) and ticks the `BitrateController` every 250 ms, pushing reported
//! targets to both the gate (`W_bytes`) and the encoder (frame sizes).
//!
//! Set `SIM_TRACE=1` to print a per-tick timeline.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use rd_flow::{BitrateConfig, BitrateController, BitrateDecision, FlowConfig, FlowGate};

const MS: u64 = 1_000;
const SEC: u64 = 1_000_000;
const MBPS: u64 = 1_000_000;

const FPS: u64 = 60;
const ENCODE_US: u64 = 4 * MS;
const DECODE_US: u64 = 3 * MS;
const TICK_US: u64 = 250 * MS;
const PING_EVERY_US: u64 = 500 * MS;
/// PING message size on the wire (24 bytes + WebSocket/TLS framing).
const PING_BYTES: u64 = 60;
const QUEUE_TARGET_US: u64 = 40 * MS;
/// Encode + decode time and scheduling slack on top of the latency bound.
const LATENCY_TOLERANCE_US: u64 = 15 * MS;

/// SplitMix64: tiny, deterministic, good enough for frame-size jitter.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[lo, hi)`.
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + (hi - lo) * unit
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Content {
    /// P-frames ~ target/fps with `frame_jitter`, keyframes `keyframe_scale` x the average.
    Moving,
    /// Static screen: tiny P-frames regardless of the target.
    Static,
}

#[derive(Debug, Clone)]
struct Scenario {
    duration_us: u64,
    one_way_delay_us: u64,
    /// `(from_us, bits/s)`, sorted, first entry at 0.
    bandwidth: Vec<(u64, u64)>,
    buffer_bytes: u64,
    bitrate: BitrateConfig,
    content: Content,
    keyframe_every_us: Option<u64>,
    keyframe_scale: f64,
    /// P-frame size = average * U(1 - jitter, 1 + jitter).
    frame_jitter: f64,
    first_seq: u32,
    seed: u64,
}

impl Scenario {
    fn new(bandwidth_bps: u64, one_way_delay_us: u64, bitrate: BitrateConfig) -> Self {
        Scenario {
            duration_us: 40 * SEC,
            one_way_delay_us,
            bandwidth: vec![(0, bandwidth_bps)],
            buffer_bytes: 4 << 20,
            bitrate,
            content: Content::Moving,
            keyframe_every_us: Some(10 * SEC),
            keyframe_scale: 10.0,
            frame_jitter: 0.5,
            first_seq: 1,
            seed: 0x5eed,
        }
    }

    fn bandwidth_at(&self, t: u64) -> u64 {
        self.bandwidth
            .iter()
            .rev()
            .find(|&&(from, _)| from <= t)
            .map_or(self.bandwidth[0].1, |&(_, bps)| bps)
    }
}

fn adaptive(start_mbps: f64, max_mbps: f64) -> BitrateConfig {
    BitrateConfig {
        start_bps: (start_mbps * MBPS as f64) as u32,
        min_bps: 500_000,
        max_bps: (max_mbps * MBPS as f64) as u32,
        adaptive: true,
    }
}

/// Bottleneck link: FIFO with a piecewise-constant bandwidth.
struct Link {
    schedule: Vec<(u64, u64)>,
    free_at_us: u64,
    /// `(departure_us, bytes)` of everything not yet fully transmitted.
    queue: VecDeque<(u64, u64)>,
    max_queued_bytes: u64,
}

impl Link {
    fn new(schedule: Vec<(u64, u64)>) -> Self {
        Link {
            schedule,
            free_at_us: 0,
            queue: VecDeque::new(),
            max_queued_bytes: 0,
        }
    }

    /// Enqueue `bytes` at `now_us`; returns the time the last bit leaves the link.
    fn send(&mut self, now_us: u64, bytes: u64) -> u64 {
        while self.queue.front().is_some_and(|&(dep, _)| dep <= now_us) {
            self.queue.pop_front();
        }
        let start = self.free_at_us.max(now_us);
        let end = self.transmit_end(start, bytes * 8);
        self.free_at_us = end;
        self.queue.push_back((end, bytes));
        let queued = self.queue.iter().map(|&(_, b)| b).sum();
        self.max_queued_bytes = self.max_queued_bytes.max(queued);
        end
    }

    /// Integrate the bandwidth schedule from `start` until `bits` have been sent.
    fn transmit_end(&self, start: u64, mut bits: u64) -> u64 {
        let mut t = start;
        loop {
            let idx = self
                .schedule
                .iter()
                .rposition(|&(from, _)| from <= t)
                .unwrap_or(0);
            let bps = self.schedule[idx].1;
            let segment_end = self.schedule.get(idx + 1).map(|&(from, _)| from);
            let needed_us = (bits * SEC).div_ceil(bps);
            match segment_end {
                Some(end) if t + needed_us > end => {
                    bits -= (end - t) * bps / SEC;
                    t = end;
                }
                _ => return t + needed_us,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Capture,
    Encoded { seq: u32, frame: usize },
    Ack { seq: u32 },
    Ping,
    Pong { sent_us: u64 },
    Tick,
    RequestKeyframe,
}

#[derive(Debug, Clone, Copy)]
struct FrameRecord {
    capture_us: u64,
    bytes: u64,
    key: bool,
    ack_us: Option<u64>,
    /// Encoder target when the frame was captured.
    target_bps: u64,
}

#[derive(Debug, Clone, Copy)]
struct TickRecord {
    t_us: u64,
    target_bps: u32,
    decision: BitrateDecision,
}

struct Outcome {
    scenario: Scenario,
    frames: Vec<FrameRecord>,
    ticks: Vec<TickRecord>,
    captured: u64,
    window_drops: u64,
    max_queued_bytes: u64,
    /// Largest `W_bytes` the gate reported.
    max_window_bytes: u64,
    max_inflight_age_us: u64,
    reported: Vec<u32>,
    last_seq: u32,
    seq_wrapped: bool,
}

fn run(sc: Scenario) -> Outcome {
    let trace = std::env::var_os("SIM_TRACE").is_some();
    if trace {
        println!(
            "== bandwidth {:?} D={} ms bitrate {:?}",
            sc.bandwidth,
            sc.one_way_delay_us / MS,
            sc.bitrate
        );
    }
    let mut rng = Rng(sc.seed);
    let gate = FlowGate::with_first_seq(
        FlowConfig {
            fps: FPS as u32,
            target_bps: sc.bitrate.start_bps,
            ..FlowConfig::default()
        },
        sc.first_seq,
    );
    let mut ctrl = BitrateController::new(sc.bitrate, 0);
    let mut encoder_bps = u64::from(ctrl.target_bps());
    gate.set_target_bps(ctrl.target_bps());
    let mut link = Link::new(sc.bandwidth.clone());
    let mut decoder_free_us = 0u64;

    let mut events = BinaryHeap::new();
    let mut order = 0u64;
    let mut schedule = |events: &mut BinaryHeap<_>, t: u64, ev: Event| {
        order += 1;
        events.push(Reverse((t, order, ev)));
    };
    schedule(&mut events, 0, Event::Capture);
    schedule(&mut events, TICK_US, Event::Tick);
    schedule(&mut events, 0, Event::Ping);
    if let Some(every) = sc.keyframe_every_us {
        schedule(&mut events, every, Event::RequestKeyframe);
    }

    let mut frames: Vec<FrameRecord> = Vec::new();
    let mut seq_to_frame: VecDeque<(u32, usize)> = VecDeque::new();
    let mut ticks = Vec::new();
    let mut reported = Vec::new();
    let mut captured = 0u64;
    let mut window_drops = 0u64;
    let mut max_inflight_age_us = 0u64;
    let mut max_window_bytes = gate.stats().window_bytes;
    let mut prev_seq: Option<u32> = None;
    let mut seq_wrapped = false;
    let mut last_acked: Option<u32> = None;

    while let Some(Reverse((now, _, ev))) = events.pop() {
        if now > sc.duration_us {
            break;
        }
        match ev {
            Event::Capture => {
                captured += 1;
                let admitted = gate.admit(now);
                ctrl.on_captured(admitted.is_none(), now);
                match admitted {
                    None => window_drops += 1,
                    Some(a) => {
                        if let Some(p) = prev_seq {
                            assert_ne!(a.seq, 0, "seq 0 is never used");
                            seq_wrapped |= a.seq < p;
                        }
                        prev_seq = Some(a.seq);
                        let key = a.force_keyframe || frames.is_empty();
                        let avg = encoder_bps as f64 / 8.0 / FPS as f64;
                        let bytes = match (sc.content, key) {
                            (Content::Moving, true) => avg * sc.keyframe_scale,
                            (Content::Moving, false) => {
                                avg * rng.uniform(1.0 - sc.frame_jitter, 1.0 + sc.frame_jitter)
                            }
                            (Content::Static, true) => 60_000.0,
                            (Content::Static, false) => rng.uniform(100.0, 400.0),
                        } as u64;
                        frames.push(FrameRecord {
                            capture_us: now,
                            bytes,
                            key,
                            ack_us: None,
                            target_bps: encoder_bps,
                        });
                        let frame = frames.len() - 1;
                        schedule(
                            &mut events,
                            now + ENCODE_US,
                            Event::Encoded { seq: a.seq, frame },
                        );
                    }
                }
                schedule(&mut events, now + SEC / FPS, Event::Capture);
            }
            Event::Encoded { seq, frame } => {
                let f = frames[frame];
                ctrl.on_encoded(f.bytes as usize, now);
                gate.on_sent(seq, f.bytes as usize, f.key, now);
                seq_to_frame.push_back((seq, frame));
                let departed = link.send(now, f.bytes);
                let decode_start = (departed + sc.one_way_delay_us).max(decoder_free_us);
                decoder_free_us = decode_start + DECODE_US;
                schedule(
                    &mut events,
                    decoder_free_us + sc.one_way_delay_us,
                    Event::Ack { seq },
                );
            }
            Event::Ack { seq } => {
                let samples = gate.on_ack(seq, DECODE_US as u32, now);
                assert!(
                    !samples.is_empty(),
                    "every ACK releases at least the acked frame"
                );
                assert_eq!(samples.last().map(|s| s.seq), Some(seq));
                for w in samples.windows(2) {
                    assert_eq!(
                        w[1].seq,
                        w[0].seq.wrapping_add(1).max(1),
                        "samples in seq order"
                    );
                }
                if let Some(prev) = last_acked {
                    assert!(
                        gate.on_ack(prev, 0, now).is_empty(),
                        "old ACK must be ignored"
                    );
                }
                last_acked = Some(seq);
                ctrl.on_ack_samples(&samples, now);
                while let Some(&(s, frame)) = seq_to_frame.front() {
                    if s.wrapping_sub(seq) as i32 > 0 {
                        break;
                    }
                    seq_to_frame.pop_front();
                    frames[frame].ack_us = Some(now);
                }
            }
            Event::Ping => {
                let departed = link.send(now, PING_BYTES);
                let pong_at = departed + 2 * sc.one_way_delay_us;
                schedule(&mut events, pong_at, Event::Pong { sent_us: now });
                schedule(&mut events, now + PING_EVERY_US, Event::Ping);
            }
            Event::Pong { sent_us } => {
                gate.on_rtt_sample(now - sent_us, now);
                max_window_bytes = max_window_bytes.max(gate.stats().window_bytes);
            }
            Event::Tick => {
                let stats = gate.stats();
                if let Some(age) = gate.oldest_inflight_age_us(now) {
                    max_inflight_age_us = max_inflight_age_us.max(age);
                }
                if let Some(bps) = ctrl.tick(now, stats.rtt_min_us) {
                    reported.push(bps);
                    gate.set_target_bps(bps);
                    encoder_bps = u64::from(bps);
                    max_window_bytes = max_window_bytes.max(gate.stats().window_bytes);
                }
                ticks.push(TickRecord {
                    t_us: now,
                    target_bps: ctrl.target_bps(),
                    decision: ctrl.last_decision(),
                });
                if trace {
                    println!(
                        "t={:6.2}s B={:5.1} target={:6.2} enc={:6.2} deliv={:6.2} encoded={:6.2} q={:4}ms rtt_min={:3}ms inflight={:2}/{:2} {:6}B/{:6}B drops={} {:?}",
                        now as f64 / SEC as f64,
                        sc.bandwidth_at(now) as f64 / MBPS as f64,
                        f64::from(ctrl.target_bps()) / MBPS as f64,
                        encoder_bps as f64 / MBPS as f64,
                        ctrl.delivery_bps() as f64 / MBPS as f64,
                        ctrl.encoded_bps() as f64 / MBPS as f64,
                        ctrl.queue_delay_us() / MS,
                        stats.rtt_min_us / MS,
                        stats.inflight_frames,
                        stats.window_frames,
                        stats.inflight_bytes,
                        stats.window_bytes,
                        stats.drop_window_total,
                        ctrl.last_decision(),
                    );
                }
                schedule(&mut events, now + TICK_US, Event::Tick);
            }
            Event::RequestKeyframe => {
                gate.request_keyframe();
                if let Some(every) = sc.keyframe_every_us {
                    schedule(&mut events, now + every, Event::RequestKeyframe);
                }
            }
        }
    }

    Outcome {
        frames,
        ticks,
        captured,
        window_drops,
        max_queued_bytes: link.max_queued_bytes,
        max_window_bytes,
        max_inflight_age_us,
        reported,
        last_seq: gate.stats().last_seq,
        seq_wrapped,
        scenario: sc,
    }
}

impl Outcome {
    /// Capture→ACK latencies of non-key frames captured in `[from, to)` and ACKed.
    fn latencies(&self, from: u64, to: u64) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .frames
            .iter()
            .filter(|f| !f.key && f.capture_us >= from && f.capture_us < to)
            .filter_map(|f| f.ack_us.map(|a| a - f.capture_us))
            .collect();
        v.sort_unstable();
        v
    }

    fn p95_latency(&self, from: u64, to: u64) -> u64 {
        let v = self.latencies(from, to);
        assert!(!v.is_empty(), "no ACKed frames in [{from}, {to})");
        v[(v.len() * 95).div_ceil(100) - 1]
    }

    /// `rtt_min + 40 ms + 2 frame serialization times + tolerance`, with the serialization of an
    /// average frame at the mean encoder target over `[from, to)` and the bandwidth at `from`.
    fn latency_bound(&self, from: u64, to: u64) -> u64 {
        let in_range: Vec<&FrameRecord> = self
            .frames
            .iter()
            .filter(|f| f.capture_us >= from && f.capture_us < to)
            .collect();
        let mean_target =
            in_range.iter().map(|f| f.target_bps).sum::<u64>() / in_range.len().max(1) as u64;
        let serialization_us = mean_target * SEC / FPS / self.scenario.bandwidth_at(from);
        2 * self.scenario.one_way_delay_us
            + QUEUE_TARGET_US
            + 2 * serialization_us
            + LATENCY_TOLERANCE_US
    }

    fn assert_latency_bounded(&self, from: u64, to: u64) {
        let p95 = self.p95_latency(from, to);
        let bound = self.latency_bound(from, to);
        assert!(
            p95 <= bound,
            "p95 latency {} ms > bound {} ms in [{:.1}, {:.1}) s",
            p95 / MS,
            bound / MS,
            from as f64 / SEC as f64,
            to as f64 / SEC as f64
        );
    }

    fn targets(&self, from: u64, to: u64) -> impl Iterator<Item = u64> + '_ {
        self.ticks
            .iter()
            .filter(move |t| t.t_us >= from && t.t_us < to)
            .map(|t| u64::from(t.target_bps))
    }

    fn target_at(&self, t_us: u64) -> u64 {
        self.ticks
            .iter()
            .rev()
            .find(|t| t.t_us <= t_us)
            .map_or(u64::from(self.scenario.bitrate.start_bps), |t| {
                u64::from(t.target_bps)
            })
    }

    /// Bits/s ACKed (delivered) for frames captured in `[from, to)`.
    fn goodput(&self, from: u64, to: u64) -> u64 {
        let bytes: u64 = self
            .frames
            .iter()
            .filter(|f| f.capture_us >= from && f.capture_us < to && f.ack_us.is_some())
            .map(|f| f.bytes)
            .sum();
        bytes * 8 * SEC / (to - from)
    }

    /// One-line digest of `[from, to)` for assertion messages and exploration.
    fn summary(&self, from: u64, to: u64) -> String {
        let targets: Vec<u64> = self.targets(from, to).collect();
        let mean = targets.iter().sum::<u64>() / targets.len().max(1) as u64;
        let b = self.scenario.bandwidth_at(from) as f64;
        format!(
            "[{:.0},{:.0})s B={:.1}: p95={}ms bound={}ms target mean={:.2}B min={:.2}B max={:.2}B goodput={:.2}B maxq={}KB drops={:.1}%",
            from as f64 / SEC as f64,
            to as f64 / SEC as f64,
            b / MBPS as f64,
            self.p95_latency(from, to) / MS,
            self.latency_bound(from, to) / MS,
            mean as f64 / b,
            targets.iter().copied().min().unwrap_or(0) as f64 / b,
            targets.iter().copied().max().unwrap_or(0) as f64 / b,
            self.goodput(from, to) as f64 / b,
            self.max_queued_bytes / 1000,
            100.0 * self.window_drops as f64 / self.captured.max(1) as f64,
        )
    }

    fn assert_no_stall(&self) {
        assert!(
            self.max_inflight_age_us < 1_500 * MS,
            "oldest in-flight frame reached {} ms",
            self.max_inflight_age_us / MS
        );
        assert!(
            self.max_queued_bytes < self.scenario.buffer_bytes,
            "bottleneck buffer overflowed"
        );
        let unacked = self
            .frames
            .iter()
            .filter(|f| f.ack_us.is_none() && f.capture_us + SEC < self.scenario.duration_us)
            .count();
        assert_eq!(unacked, 0, "frames sent but never ACKed");
    }
}

fn mbps(bps: u64) -> f64 {
    bps as f64 / MBPS as f64
}

/// `bps` raised by `n` controller increase steps (x1.06 + 200 kbps each). Increases come every
/// 500 ms, so the target can overshoot the link rate by a few steps before the window drops
/// they cause make it back off.
fn steps_above(bps: u64, n: u32) -> u64 {
    (0..n).fold(bps as f64, |b, _| b * 1.06 + 200_000.0) as u64
}

// ---------------------------------------------------------------------------------------------

/// (1) Even with a 4 MB bottleneck buffer the queueing delay stays bounded: the window (not the
/// buffer) limits what is in flight.
#[test]
fn steady_state_latency_is_bounded_despite_deep_buffer() {
    for (bw_mbps, d_ms) in [(20, 20), (8, 5), (50, 40), (3, 10)] {
        let sc = Scenario::new(bw_mbps * MBPS, d_ms * MS, adaptive(4.0, 100.0));
        let out = run(sc);
        out.assert_no_stall();
        out.assert_latency_bounded(10 * SEC, 40 * SEC);
        // The window, not the 4 MB buffer, bounds the queue: at most a window plus the frame
        // admitted into it (plus a PING).
        let largest_frame = out.frames.iter().map(|f| f.bytes).max().unwrap_or(0);
        assert!(
            out.max_queued_bytes <= out.max_window_bytes + largest_frame + PING_BYTES,
            "{bw_mbps} Mbps: queue reached {} bytes (window {}, frame {largest_frame})",
            out.max_queued_bytes,
            out.max_window_bytes
        );
        // And still uses the link reasonably well.
        let goodput = out.goodput(10 * SEC, 39 * SEC);
        assert!(
            goodput as f64 >= 0.6 * (bw_mbps * MBPS) as f64,
            "{bw_mbps} Mbps: goodput only {:.2} Mbps",
            mbps(goodput)
        );
    }
}

/// (2) From a low start (B/16) the target converges into 0.7–1.0 x B within ~10 s and stays
/// there.
#[test]
fn target_converges_from_low_start() {
    for (bw_mbps, d_ms) in [(20, 20), (10, 15), (40, 30), (4, 25)] {
        let b = bw_mbps * MBPS;
        let mut sc = Scenario::new(b, d_ms * MS, adaptive(bw_mbps as f64 / 16.0, 200.0));
        sc.duration_us = 60 * SEC;
        let out = run(sc);
        out.assert_no_stall();
        let summary = out.summary(10 * SEC, 60 * SEC);
        let bf = b as f64;
        assert!(
            out.target_at(10 * SEC) as f64 >= 0.7 * bf,
            "not converged by 10 s: {summary}"
        );
        let targets: Vec<u64> = out.targets(10 * SEC, 60 * SEC).collect();
        let mean = targets.iter().sum::<u64>() as f64 / targets.len() as f64;
        assert!(
            (0.7 * bf..=1.0 * bf).contains(&mean),
            "mean target off: {summary}"
        );
        // Individual ticks may briefly sit above B; the window then drops the excess frames
        // and the latency bound still holds.
        let band = (0.7 * bf) as u64..=steps_above(b, 3);
        assert!(
            targets.iter().all(|t| band.contains(t)),
            "target left the band: {summary}"
        );
        out.assert_latency_bounded(10 * SEC, 60 * SEC);
        assert!(
            out.goodput(10 * SEC, 59 * SEC) as f64 >= 0.65 * bf,
            "goodput: {summary}"
        );
    }
}

/// (3) Bandwidth steps 20 -> 5 -> 20 Mbps: latency is back under the bound within 2 s of each
/// step and the target follows the link.
#[test]
fn recovers_from_bandwidth_steps() {
    let mut sc = Scenario::new(20 * MBPS, 20 * MS, adaptive(8.0, 100.0));
    sc.duration_us = 70 * SEC;
    sc.bandwidth = vec![(0, 20 * MBPS), (20 * SEC, 5 * MBPS), (40 * SEC, 20 * MBPS)];
    let out = run(sc);
    out.assert_no_stall();

    out.assert_latency_bounded(10 * SEC, 20 * SEC);
    out.assert_latency_bounded(22 * SEC, 40 * SEC);
    out.assert_latency_bounded(42 * SEC, 70 * SEC);

    // Down: cut to (just above) the new rate within 2 s, then stay around it.
    let down = out.target_at(22 * SEC);
    assert!(
        down <= 5 * MBPS * 11 / 10,
        "{:.2} Mbps 2 s after the drop",
        mbps(down)
    );
    let band = 5 * MBPS * 6 / 10..=steps_above(5 * MBPS, 3);
    assert!(
        out.targets(25 * SEC, 40 * SEC).all(|t| band.contains(&t)),
        "{}",
        out.summary(25 * SEC, 40 * SEC)
    );

    // Up: ramps back to >= 0.7 x B within 15 s.
    let up = out.target_at(55 * SEC);
    assert!(
        up >= 14 * MBPS,
        "only {:.2} Mbps 15 s after the rise",
        mbps(up)
    );
    assert!(
        out.targets(55 * SEC, 70 * SEC)
            .all(|t| t <= steps_above(20 * MBPS, 3))
    );
}

/// (4) A keyframe far larger than `W_bytes` is admitted (nothing in flight) and never deadlocks
/// the window.
#[test]
fn huge_keyframes_never_deadlock() {
    let mut sc = Scenario::new(5 * MBPS, 20 * MS, adaptive(5.0, 5.0));
    sc.duration_us = 20 * SEC;
    sc.keyframe_every_us = Some(SEC);
    sc.keyframe_scale = 40.0; // ~417 KB vs W_bytes ~50 KB
    let out = run(sc);
    out.assert_no_stall();
    let keys: Vec<&FrameRecord> = out.frames.iter().filter(|f| f.key).collect();
    assert!(keys.len() >= 18, "only {} keyframes", keys.len());
    let window_bytes = 5 * MBPS / 8 * (40 * MS + QUEUE_TARGET_US) / SEC;
    assert!(keys.iter().all(|k| k.bytes > 5 * window_bytes));
    assert!(keys.iter().all(|k| k.ack_us.is_some()));
    // P-frames keep flowing between keyframes.
    for k in &keys {
        let next = out
            .frames
            .iter()
            .find(|f| !f.key && f.capture_us > k.capture_us)
            .expect("a P-frame after every keyframe");
        assert!(next.capture_us - k.capture_us < 900 * MS);
    }
}

/// (5) A static screen (tiny frames) is app-limited: the target must not ramp.
#[test]
fn static_screen_does_not_ramp() {
    let mut sc = Scenario::new(20 * MBPS, 20 * MS, adaptive(4.0, 50.0));
    sc.content = Content::Static;
    sc.duration_us = 30 * SEC;
    let out = run(sc);
    out.assert_no_stall();
    let start = u64::from(out.scenario.bitrate.start_bps);
    assert!(out.targets(0, 30 * SEC).all(|t| t <= start));
    assert!(out.ticks.iter().all(|t| !matches!(
        t.decision,
        BitrateDecision::Increase | BitrateDecision::Startup
    )));
}

/// (6) Sequence numbers wrap past u32::MAX (skipping 0) without disturbing the window.
#[test]
fn seq_wraparound() {
    let mut sc = Scenario::new(20 * MBPS, 20 * MS, adaptive(8.0, 100.0));
    sc.duration_us = 20 * SEC;
    sc.first_seq = u32::MAX - 300;
    let out = run(sc);
    out.assert_no_stall();
    assert!(out.seq_wrapped, "seq did not wrap");
    assert!(out.last_seq < 2_000, "last seq {}", out.last_seq);
    out.assert_latency_bounded(10 * SEC, 20 * SEC);
}

/// (7) Fixed mode: the target never moves and nothing is reported.
#[test]
fn fixed_mode_keeps_target() {
    let fixed = |bps: u32| BitrateConfig {
        start_bps: bps,
        min_bps: 1_000_000,
        max_bps: 20_000_000,
        adaptive: false,
    };

    // Below the link rate: behaves like any well-provisioned session.
    let mut sc = Scenario::new(5 * MBPS, 20 * MS, fixed(4_000_000));
    sc.duration_us = 30 * SEC;
    let out = run(sc);
    out.assert_no_stall();
    assert!(
        out.reported.is_empty(),
        "fixed mode reported {:?}",
        out.reported
    );
    assert!(out.targets(0, 30 * SEC).all(|t| t == 4 * MBPS));
    assert!(
        out.ticks
            .iter()
            .all(|t| t.decision == BitrateDecision::Hold)
    );
    out.assert_latency_bounded(5 * SEC, 30 * SEC);

    // Above the link rate (8 vs 5 Mbps): the target still stays put; the window drops frames
    // and keeps the deep buffer empty. The standing queue this creates delays the in-band PINGs,
    // so rtt_min (and the window) can grow up to the `max_frames` cap: latency is bounded by
    // that cap rather than by `rtt_min + 40 ms`.
    let mut sc = Scenario::new(5 * MBPS, 20 * MS, fixed(8_000_000));
    sc.duration_us = 60 * SEC;
    let out = run(sc);
    out.assert_no_stall();
    assert!(
        out.reported.is_empty(),
        "fixed mode reported {:?}",
        out.reported
    );
    assert!(out.targets(0, 60 * SEC).all(|t| t == 8 * MBPS));
    assert!(out.frames.iter().all(|f| f.target_bps == 8 * MBPS));
    assert!(
        out.window_drops * 5 > out.captured,
        "the window must drop the excess"
    );
    let frame_us = 8 * SEC / FPS / 5;
    let cap_bound = 2 * 20 * MS + 16 * frame_us + LATENCY_TOLERANCE_US;
    let p95 = out.p95_latency(5 * SEC, 60 * SEC);
    assert!(
        p95 <= cap_bound,
        "p95 {} ms > {} ms",
        p95 / MS,
        cap_bound / MS
    );
    assert!(out.max_queued_bytes < 16 * 8 * MBPS / 8 / FPS * 2);
}

/// Parameter sweep for re-tuning (not an assertion): prints one line per scenario.
/// `cargo test -p rd-flow --test sim sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn sweep() {
    for (bw, d) in [
        (2, 50),
        (3, 10),
        (8, 5),
        (10, 15),
        (20, 20),
        (40, 30),
        (50, 40),
        (100, 5),
    ] {
        for jitter in [0.0, 0.5, 0.9] {
            let mut sc = Scenario::new(bw * MBPS, d * MS, adaptive(1.0, 200.0));
            sc.duration_us = 60 * SEC;
            sc.frame_jitter = jitter;
            let out = run(sc);
            println!(
                "B={bw:3} D={d:2} jitter={jitter:.1} startup {} | steady {}",
                out.summary(3 * SEC, 10 * SEC),
                out.summary(10 * SEC, 60 * SEC)
            );
        }
    }
}
