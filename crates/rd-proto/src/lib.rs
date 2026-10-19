//! rd wire protocol, version 1.
//!
//! Two kinds of WebSocket messages travel between viewer, gateway and agent:
//! - **text frames**: JSON control messages, tagged by a `"t"` field ([`control`]).
//! - **binary frames**: compact little-endian media / input / ack / ping frames ([`bin`]).
//!
//! The gateway never parses viewer<->agent media; it only peeks at the first byte.
//! The full spec lives in `docs/protocol.md`. [`setup_code`] is the agent onboarding string the
//! gateway issues and the agent accepts.

pub mod bin;
pub mod control;
pub mod keymap;
pub mod setup_code;

/// Viewer <-> agent protocol version spoken by this build.
pub const PROTO_VERSION: u16 = 1;
/// Oldest viewer <-> agent protocol version this build accepts.
pub const MIN_PROTO_VERSION: u16 = 1;
/// Agent <-> gateway control protocol version.
pub const GW_PROTO_VERSION: u16 = 1;

/// WebSocket close codes used by the gateway and agent.
pub mod close {
    /// Another viewer took over this machine. Viewer must NOT auto-reconnect.
    pub const KICKED: u16 = 4001;
    /// Viewer is not allowed (auth/allowlist/origin).
    pub const FORBIDDEN: u16 = 4002;
    /// Agent for this machine is offline.
    pub const AGENT_OFFLINE: u16 = 4003;
    /// Agent did not open its session connection in time.
    pub const AGENT_TIMEOUT: u16 = 4004;
    /// Protocol violation / version mismatch.
    pub const PROTOCOL: u16 = 4005;
    /// Session ended by the agent (e.g. stall, capture failure).
    pub const SESSION_ENDED: u16 = 4006;
    /// No user input for too long.
    pub const IDLE_TIMEOUT: u16 = 4008;
    /// An agent connection was replaced by a newer one for the same machine.
    pub const REPLACED: u16 = 4009;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("empty message")]
    Empty,
    #[error("message type 0x{ty:02x} too short: {len} < {need}")]
    TooShort { ty: u8, len: usize, need: usize },
    #[error("invalid value for {field}: {value}")]
    InvalidValue { field: &'static str, value: u32 },
}
