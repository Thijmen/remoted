//! Platform-neutral rd agent.
//!
//! - [`platform`]: traits a platform crate (rd-platform-macos, later rd-platform-windows) implements.
//! - [`runtime`]: gateway control connection + per-session data connection, flow control glue.
//! - [`h264`]: avcC / codec-string / Annex-B helpers.
//! - [`clock`]: the agent's monotonic microsecond clock (all wire timestamps use it).
//! - [`config`]: agent configuration file.

pub mod clock;
pub mod config;
pub mod h264;
pub mod platform;
pub mod runtime;

pub use platform::*;
