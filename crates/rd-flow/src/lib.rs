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
//!   [`FlowGate::on_encode_time`] and [`FlowGate::on_sent`]) or drops it
//!   ([`FlowGate::cancel`]).
//! - The **viewer** ACKs cumulatively after decode output: [`FlowGate::on_ack`] releases all
//!   frames with `seq <= acked` and returns per-frame samples for the [`BitrateController`].
//! - Window: `horizon = rtt_min + enc + queue_target`,
//!   `W_frames = clamp(ceil(fps * horizon) + 1, min_frames, max_frames)`,
//!   `W_bytes = target_bps / 8 * horizon`, where `enc` is the median of the last 64 non-key
//!   encode times clamped to `min(max_encode_us, 1e6 / fps)` (frames count from admission, so
//!   their time in the encoder must fit in the budget too). A frame is ALWAYS admitted when
//!   nothing is in flight (so a large keyframe can never deadlock the window).
//!
//! Sequence numbers are `u32`, start at 1, and increase by one per admitted frame for the
//! lifetime of a session (across epochs). Comparisons use wrapping arithmetic; 0 is skipped on
//! wrap-around, so a seq is never 0.

//!
//! File transfers use a separate, low-priority window: [`BulkWindow`] (docs/files.md).

mod bitrate;
mod bulk;
mod gate;
mod rtt;

pub use bitrate::{BitrateConfig, BitrateController, BitrateDecision};
pub use bulk::{BulkConfig, BulkStats, BulkWindow, TARGET_FULL_US, TARGET_GENTLE_US};
pub use gate::{AckSample, Admitted, FlowConfig, FlowGate, FlowStats};
pub use rtt::RttTracker;
