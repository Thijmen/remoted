//! [`BulkWindow`]: the send window of a file transfer (docs/files.md, Flow control).
//!
//! A low-priority, delay-based congestion window in the spirit of LEDBAT (RFC 6817). Every
//! acknowledged chunk gives a round-trip sample; the lowest one of the last two minutes is the
//! path's base delay, and whatever a transfer adds on top of it is queueing it causes. The
//! window grows while that queue is below the target and shrinks above it, so a transfer fills
//! an idle path but leaves room when the screen stream needs it.
//!
//! The viewer has a TypeScript copy (`viewer/src/files/bulk.ts`); keep them in step.

use std::collections::VecDeque;

/// Tuning of a [`BulkWindow`]. The defaults are the values in docs/files.md.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkConfig {
    pub min_window: u64,
    pub max_window: u64,
    pub initial_window: u64,
    pub min_chunk: u64,
    pub max_chunk: u64,
    /// How long the base delay remembers its minimum.
    pub base_history_us: u64,
    /// Granularity of that memory.
    pub base_bucket_us: u64,
    /// The current delay is the minimum of this many recent samples (filters out jitter).
    pub recent_samples: usize,
}

impl Default for BulkConfig {
    fn default() -> Self {
        BulkConfig {
            min_window: 16 << 10,
            max_window: 16 << 20,
            initial_window: 64 << 10,
            min_chunk: 16 << 10,
            max_chunk: 256 << 10,
            base_history_us: 120_000_000,
            base_bucket_us: 10_000_000,
            recent_samples: 4,
        }
    }
}

/// Queueing a transfer may cause while a screen session is open.
pub const TARGET_GENTLE_US: u64 = 10_000;
/// Queueing a transfer may cause otherwise.
pub const TARGET_FULL_US: u64 = 80_000;

#[derive(Debug, Clone, Copy)]
struct InFlight {
    seq: u32,
    bytes: u64,
    sent_us: u64,
}

/// What a [`BulkWindow`] knows, for logs and the viewer's transfer details.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BulkStats {
    pub window: u64,
    pub inflight_bytes: u64,
    pub chunk: u64,
    pub base_rtt_us: Option<u64>,
    pub rtt_us: Option<u64>,
    /// Queueing delay the transfer causes (`rtt − base`).
    pub queue_us: Option<u64>,
    pub target_us: u64,
    pub slow_start: bool,
}

/// The send window of one side of a file channel. All transfers in that direction share it.
#[derive(Debug, Clone)]
pub struct BulkWindow {
    cfg: BulkConfig,
    target_us: u64,
    window: f64,
    slow_start: bool,
    next_seq: u32,
    inflight: VecDeque<InFlight>,
    inflight_bytes: u64,
    /// (bucket start, lowest rtt in it), oldest first.
    base: VecDeque<(u64, u64)>,
    recent: VecDeque<u64>,
    last_cut_us: Option<u64>,
    /// Most bytes in flight lately, and when: was the window what limited the sender?
    peak_inflight: u64,
    peak_us: u64,
}

impl BulkWindow {
    pub fn new(cfg: BulkConfig, target_us: u64) -> Self {
        let window = cfg.initial_window as f64;
        BulkWindow {
            cfg,
            target_us: target_us.max(1),
            window,
            slow_start: true,
            next_seq: 1,
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            base: VecDeque::new(),
            recent: VecDeque::new(),
            last_cut_us: None,
            peak_inflight: 0,
            peak_us: 0,
        }
    }

    /// Changes the queueing target ([`TARGET_GENTLE_US`] while a screen session is open).
    pub fn set_target(&mut self, target_us: u64) {
        self.target_us = target_us.max(1);
    }

    pub fn target_us(&self) -> u64 {
        self.target_us
    }

    /// Bytes the next frame should carry: a quarter of the window, at least 16 KiB, at most
    /// 256 KiB, in whole KiB.
    pub fn chunk_size(&self) -> u64 {
        let quarter = (self.window as u64 / 4) & !1023;
        quarter.clamp(self.cfg.min_chunk, self.cfg.max_chunk)
    }

    /// Whether a frame of `bytes` may go now. Always when nothing is in flight.
    pub fn can_send(&self, bytes: u64) -> bool {
        self.inflight.is_empty() || self.inflight_bytes + bytes <= self.window as u64
    }

    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes
    }

    /// Nothing is waiting for an acknowledgement.
    pub fn idle(&self) -> bool {
        self.inflight.is_empty()
    }

    /// A frame of `bytes` was handed to the socket; returns its `seq`.
    pub fn on_sent(&mut self, bytes: u64, now_us: u64) -> u32 {
        let seq = self.next_seq;
        self.next_seq = match self.next_seq.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.inflight_bytes += bytes;
        if self.inflight_bytes >= self.peak_inflight {
            self.peak_inflight = self.inflight_bytes;
            self.peak_us = now_us;
        }
        self.inflight.push_back(InFlight {
            seq,
            bytes,
            sent_us: now_us,
        });
        seq
    }

    /// A cumulative FILE_ACK: every frame up to and including `seq` arrived. Returns the bytes
    /// it released. Acks for frames not in flight (duplicates, stale) release nothing.
    pub fn on_ack(&mut self, seq: u32, now_us: u64) -> u64 {
        if !self.inflight.iter().any(|f| f.seq == seq) {
            return 0;
        }
        let mut released = 0;
        while let Some(f) = self.inflight.pop_front() {
            self.inflight_bytes -= f.bytes;
            released += f.bytes;
            self.on_sample(f, now_us);
            if f.seq == seq {
                break;
            }
        }
        released
    }

    /// The connection was replaced: frames in flight will never be acknowledged. The window and
    /// delay history start over (it may be another path).
    pub fn reset(&mut self) {
        let target = self.target_us;
        *self = BulkWindow::new(self.cfg.clone(), target);
    }

    pub fn stats(&self) -> BulkStats {
        let base = self.base_rtt();
        let rtt = self.recent.iter().copied().min();
        BulkStats {
            window: self.window as u64,
            inflight_bytes: self.inflight_bytes,
            chunk: self.chunk_size(),
            base_rtt_us: base,
            rtt_us: rtt,
            queue_us: base.zip(rtt).map(|(b, r)| r.saturating_sub(b)),
            target_us: self.target_us,
            slow_start: self.slow_start,
        }
    }

    fn base_rtt(&self) -> Option<u64> {
        self.base.iter().map(|&(_, rtt)| rtt).min()
    }

    fn on_sample(&mut self, f: InFlight, now_us: u64) {
        let rtt = now_us.saturating_sub(f.sent_us).max(1);
        self.add_base(rtt, now_us);
        self.recent.push_back(rtt);
        while self.recent.len() > self.cfg.recent_samples {
            self.recent.pop_front();
        }
        let current = self.recent.iter().copied().min().unwrap_or(rtt);
        let queue = current.saturating_sub(self.base_rtt().unwrap_or(current));
        let target = self.target_us as f64;
        // Grow only while the window is what limits the sender (not the disk, not the user):
        // it had at least half of it in flight within the last two round trips.
        if now_us.saturating_sub(self.peak_us) > 2 * current {
            self.peak_inflight = self.inflight_bytes;
            self.peak_us = now_us;
        }
        let limited = self.peak_inflight as f64 >= self.window * 0.5;

        if queue > 2 * self.target_us
            && self
                .last_cut_us
                .is_none_or(|t| now_us.saturating_sub(t) >= current)
        {
            self.window /= 2.0;
            self.slow_start = false;
            self.last_cut_us = Some(now_us);
        } else if self.slow_start {
            if queue as f64 > target / 2.0 {
                self.slow_start = false;
            } else if limited {
                self.window += f.bytes as f64;
            }
        } else {
            let off = ((target - queue as f64) / target).clamp(-1.0, 1.0);
            if off < 0.0 || limited {
                let chunk = self.chunk_size() as f64;
                self.window += off * f.bytes as f64 * chunk / self.window;
            }
        }
        self.window = self
            .window
            .clamp(self.cfg.min_window as f64, self.cfg.max_window as f64);
    }

    fn add_base(&mut self, rtt: u64, now_us: u64) {
        let bucket = now_us - now_us % self.cfg.base_bucket_us.max(1);
        match self.base.back_mut() {
            Some((start, min)) if *start == bucket => *min = (*min).min(rtt),
            _ => self.base.push_back((bucket, rtt)),
        }
        while self
            .base
            .front()
            .is_some_and(|&(start, _)| start + self.cfg.base_history_us <= bucket)
        {
            self.base.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    fn window() -> BulkWindow {
        BulkWindow::new(BulkConfig::default(), TARGET_FULL_US)
    }

    #[test]
    fn sends_one_frame_even_when_it_is_larger_than_the_window() {
        let mut w = window();
        assert!(w.can_send(1 << 20));
        w.on_sent(1 << 20, 0);
        assert!(!w.can_send(1));
        assert_eq!(w.on_ack(1, 10 * MS), 1 << 20);
        assert!(w.idle());
    }

    #[test]
    fn cumulative_acks_and_seq_wrap() {
        let mut w = window();
        w.next_seq = u32::MAX;
        let a = w.on_sent(100, 0);
        let b = w.on_sent(100, 0);
        let c = w.on_sent(100, 0);
        assert_eq!((a, b, c), (u32::MAX, 1, 2));
        assert_eq!(w.on_ack(b, MS), 200);
        assert_eq!(w.on_ack(b, MS), 0, "duplicate");
        assert_eq!(w.on_ack(77, MS), 0, "never sent");
        assert_eq!(w.inflight_bytes(), 100);
        assert_eq!(w.on_ack(c, MS), 100);
    }

    #[test]
    fn slow_start_doubles_per_round_trip_while_there_is_no_queue() {
        let mut w = window();
        let start = w.stats().window;
        // One window's worth sent and acknowledged after 20 ms: the window doubles.
        let mut t = 0;
        let mut sent = 0;
        while w.can_send(w.chunk_size()) {
            w.on_sent(w.chunk_size(), t);
            sent += 1;
        }
        t += 20 * MS;
        for seq in 1..=sent {
            w.on_ack(seq, t);
        }
        assert_eq!(w.stats().window, 2 * start);
        assert!(w.stats().slow_start);
    }

    #[test]
    fn a_growing_queue_ends_slow_start_and_shrinks_the_window() {
        let mut w = BulkWindow::new(BulkConfig::default(), TARGET_GENTLE_US);
        let mut t = 0;
        let mut seq = 0;
        let mut rtt = 10 * MS;
        for _ in 0..200 {
            let chunk = w.chunk_size();
            while w.can_send(chunk) {
                seq = w.on_sent(chunk, t);
            }
            t += rtt;
            w.on_ack(seq, t);
            // Every round adds 10 ms of queue.
            rtt += 10 * MS;
            if rtt > 300 * MS {
                break;
            }
        }
        let s = w.stats();
        assert!(!s.slow_start);
        assert_eq!(s.window, BulkConfig::default().min_window, "{s:?}");
    }

    #[test]
    fn app_limited_senders_do_not_inflate_the_window() {
        let mut w = window();
        let start = w.stats().window;
        // One small frame at a time: the window is never the limit.
        for i in 1..=1000u64 {
            let seq = w.on_sent(1024, i * 30 * MS);
            w.on_ack(seq, i * 30 * MS + 10 * MS);
        }
        assert_eq!(w.stats().window, start);
    }

    #[test]
    fn base_delay_forgets_after_two_minutes() {
        let mut w = window();
        let seq = w.on_sent(10, 0);
        w.on_ack(seq, 5 * MS);
        for i in 1..=13u64 {
            let t = i * 10_000 * MS;
            let seq = w.on_sent(10, t);
            w.on_ack(seq, t + 50 * MS);
        }
        // The 5 ms sample (bucket 0) is gone after 120 s.
        assert_eq!(w.stats().base_rtt_us, Some(50 * MS));
    }

    #[test]
    fn chunk_follows_the_window() {
        let mut w = window();
        assert_eq!(w.chunk_size(), 16 << 10);
        w.window = (4 << 20) as f64;
        assert_eq!(w.chunk_size(), 256 << 10);
        w.window = (200 << 10) as f64;
        assert_eq!(w.chunk_size(), 50 << 10);
    }
}
