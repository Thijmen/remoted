//! Windowed minimum RTT tracker (min over the last `window_us`).

/// Keeps RTT samples for `window_us` and reports the minimum.
#[derive(Debug, Clone)]
pub struct RttTracker {
    #[allow(dead_code)]
    window_us: u64,
}

impl RttTracker {
    /// `window_us` default used by FlowGate: 10_000_000 (10 s).
    pub fn new(window_us: u64) -> Self {
        let _ = window_us;
        todo!("rd-flow: RttTracker")
    }
    pub fn add(&mut self, rtt_us: u64, now_us: u64) {
        let _ = (rtt_us, now_us);
        todo!()
    }
    /// Minimum over the window (samples older than `now_us - window_us` expire), or the most
    /// recent sample if all expired, or None if never fed.
    pub fn min(&self, now_us: u64) -> Option<u64> {
        let _ = now_us;
        todo!()
    }
    /// Median of samples in the window, if any.
    pub fn median(&self, now_us: u64) -> Option<u64> {
        let _ = now_us;
        todo!()
    }
}
