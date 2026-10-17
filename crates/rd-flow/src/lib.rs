//! Pure flow-control logic for rd. No I/O, no async, no clocks: every method takes `now_us`
//! (a monotonic microsecond clock supplied by the caller), so everything is unit-testable.
//!
//! Used by the agent (rd-agent-core) for real video, and by the gateway's `/probe` windowed
//! soak test with synthetic frames.
//!
//! ## Model
//! - The **capture thread** calls [`FlowGate::admit`] for every captured frame. If the
//!   in-flight window is full (or paused) the frame is dropped *before encode* (the H.264
//!   reference chain stays valid). Otherwise it gets the next `seq`.
//! - The **encoder** either produces the frame (network task later calls
//!   [`FlowGate::on_sent`]) or drops it ([`FlowGate::cancel`]).
//! - The **viewer** ACKs cumulatively after decode output: [`FlowGate::on_ack`] releases all
//!   frames with `seq <= acked` and returns per-frame samples for the [`BitrateController`].
//! - Window: `W_frames = clamp(ceil(fps * (rtt_min + queue_target)) + 1, min_frames, max_frames)`,
//!   `W_bytes = target_bps / 8 * (rtt_min + queue_target)`. A frame is ALWAYS admitted when
//!   nothing is in flight (so a large keyframe can never deadlock the window).
//!
//! Sequence numbers are `u32`, start at 1, and increase by one per admitted frame for the
//! lifetime of a session (across epochs). Comparisons use wrapping arithmetic; 0 is skipped on
//! wrap-around, so a seq is never 0.

mod bitrate;
mod gate;
mod rtt;

pub use bitrate::{BitrateConfig, BitrateController, BitrateDecision};
pub use gate::{AckSample, Admitted, FlowConfig, FlowGate, FlowStats};
pub use rtt::RttTracker;
