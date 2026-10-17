//! Windowed minimum RTT tracker (min over the last `window_us`).

use std::collections::VecDeque;

/// Hard cap on stored samples, so a caller that never advances its clock cannot grow the
/// tracker without bound. Far above the expected rate (one PING every 500 ms ⇒ 20 per 10 s).
const MAX_SAMPLES: usize = 1024;

/// Keeps RTT samples for `window_us` and reports the minimum.
#[derive(Debug, Clone)]
pub struct RttTracker {
    window_us: u64,
    /// `(time_us, rtt_us)`, oldest first. Expired samples are pruned on `add`; the newest is
    /// never pruned, so `min` can fall back to it.
    samples: VecDeque<(u64, u64)>,
}

impl RttTracker {
    /// `window_us` default used by FlowGate: 10_000_000 (10 s).
    pub fn new(window_us: u64) -> Self {
        RttTracker {
            window_us,
            samples: VecDeque::new(),
        }
    }

    pub fn add(&mut self, rtt_us: u64, now_us: u64) {
        // Every expired sample can go: the new one becomes the "most recent" fallback.
        let horizon = self.horizon(now_us);
        while self.samples.front().is_some_and(|&(t, _)| t < horizon) {
            self.samples.pop_front();
        }
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now_us, rtt_us));
    }

    /// Minimum over the window (samples older than `now_us - window_us` expire), or the most
    /// recent sample if all expired, or None if never fed.
    pub fn min(&self, now_us: u64) -> Option<u64> {
        let horizon = self.horizon(now_us);
        self.live(horizon)
            .min()
            .or_else(|| self.samples.back().map(|&(_, rtt)| rtt))
    }

    /// Median of samples in the window, if any.
    pub fn median(&self, now_us: u64) -> Option<u64> {
        let mut live: Vec<u64> = self.live(self.horizon(now_us)).collect();
        if live.is_empty() {
            return None;
        }
        let mid = live.len() / 2;
        let (_, median, _) = live.select_nth_unstable(mid);
        Some(*median)
    }

    /// Forget all samples.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    fn horizon(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.window_us)
    }

    fn live(&self, horizon: u64) -> impl Iterator<Item = u64> + '_ {
        self.samples
            .iter()
            .filter(move |&&(t, _)| t >= horizon)
            .map(|&(_, rtt)| rtt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn empty_tracker_reports_nothing() {
        let t = RttTracker::new(10 * S);
        assert_eq!(t.min(0), None);
        assert_eq!(t.median(0), None);
    }

    #[test]
    fn min_over_window() {
        let mut t = RttTracker::new(10 * S);
        t.add(50_000, 0);
        t.add(30_000, S);
        t.add(70_000, 2 * S);
        assert_eq!(t.min(2 * S), Some(30_000));
        // The 30 ms sample (t = 1 s) expires after t = 11 s.
        assert_eq!(t.min(11 * S), Some(30_000));
        assert_eq!(t.min(11 * S + 1), Some(70_000));
    }

    #[test]
    fn min_falls_back_to_latest_when_all_expired() {
        let mut t = RttTracker::new(10 * S);
        t.add(20_000, 0);
        t.add(40_000, S);
        assert_eq!(t.min(100 * S), Some(40_000));
        assert_eq!(t.median(100 * S), None);
    }

    #[test]
    fn add_prunes_expired_samples() {
        let mut t = RttTracker::new(10 * S);
        t.add(10_000, 0);
        t.add(90_000, 20 * S);
        // The old 10 ms sample must not survive as the fallback or in the window.
        assert_eq!(t.min(20 * S), Some(90_000));
        assert_eq!(t.samples.len(), 1);
    }

    #[test]
    fn median_odd_and_even() {
        let mut t = RttTracker::new(10 * S);
        for (i, rtt) in [50, 10, 40, 20, 30].into_iter().enumerate() {
            t.add(rtt, i as u64);
        }
        assert_eq!(t.median(10), Some(30));
        t.add(60, 6);
        // Upper median for an even count.
        assert_eq!(t.median(10), Some(40));
    }

    #[test]
    fn median_ignores_expired() {
        let mut t = RttTracker::new(S);
        t.add(1, 0);
        t.add(100, 2 * S);
        t.add(200, 2 * S + 1);
        assert_eq!(t.median(2 * S + 1), Some(200));
    }

    #[test]
    fn bounded_when_clock_stands_still() {
        let mut t = RttTracker::new(10 * S);
        for i in 0..(MAX_SAMPLES as u64 + 10) {
            t.add(1_000 + i, 5);
        }
        assert_eq!(t.samples.len(), MAX_SAMPLES);
        assert_eq!(t.min(5), Some(1_010));
    }

    #[test]
    fn clock_before_window_saturates() {
        let mut t = RttTracker::new(10 * S);
        t.add(42, 3);
        assert_eq!(t.min(4), Some(42));
        assert_eq!(t.median(4), Some(42));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut t = RttTracker::new(10 * S);
        t.add(42, 0);
        t.clear();
        assert_eq!(t.min(0), None);
    }
}
