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

#[derive(Debug, Clone)]
pub struct BitrateController {
    #[allow(dead_code)]
    cfg: BitrateConfig,
}

impl BitrateController {
    pub fn new(cfg: BitrateConfig, now_us: u64) -> Self {
        let _ = (cfg, now_us);
        todo!("rd-flow: BitrateController")
    }
    /// Change limits / mode (viewer `set`). Target is re-clamped.
    pub fn set_config(&mut self, cfg: BitrateConfig) {
        let _ = cfg;
        todo!()
    }
    /// Feed ACK samples from [`crate::FlowGate::on_ack`].
    pub fn on_ack_samples(&mut self, samples: &[crate::AckSample], now_us: u64) {
        let _ = (samples, now_us);
        todo!()
    }
    /// An encoded frame of `bytes` was produced (app-limited detection).
    pub fn on_encoded(&mut self, bytes: usize, now_us: u64) {
        let _ = (bytes, now_us);
        todo!()
    }
    /// A frame was captured (`dropped_by_window` = gate returned None).
    pub fn on_captured(&mut self, dropped_by_window: bool, now_us: u64) {
        let _ = (dropped_by_window, now_us);
        todo!()
    }
    /// Run the control law; call every 250 ms. Returns `Some(new_target_bps)` only when the
    /// target changed by more than 5 % since the last reported value.
    pub fn tick(&mut self, now_us: u64, rtt_min_us: u64) -> Option<u32> {
        let _ = (now_us, rtt_min_us);
        todo!()
    }
    pub fn target_bps(&self) -> u32 {
        todo!()
    }
    pub fn last_decision(&self) -> BitrateDecision {
        todo!()
    }
    /// Current queueing-delay estimate `q` in microseconds (for stats).
    pub fn queue_delay_us(&self) -> u64 {
        todo!()
    }
    /// Delivery rate over the last second, bits/s.
    pub fn delivery_bps(&self) -> u64 {
        todo!()
    }
    /// Encoded bitrate over the last second, bits/s.
    pub fn encoded_bps(&self) -> u64 {
        todo!()
    }
}
