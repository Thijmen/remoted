//! The agent's monotonic clock. All timestamps on the wire from the agent (VIDEO capture_us,
//! PING/PONG) use this clock, so viewers can derive a clock offset from PING/PONG.

use std::sync::OnceLock;
use std::time::Instant;

static ANCHOR: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the first call in this process (monotonic).
pub fn now_us() -> u64 {
    ANCHOR.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Convert an `Instant` to the agent clock.
pub fn instant_to_us(t: Instant) -> u64 {
    let a = *ANCHOR.get_or_init(Instant::now);
    t.saturating_duration_since(a).as_micros() as u64
}
