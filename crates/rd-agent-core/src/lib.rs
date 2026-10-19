//! Platform-neutral rd agent.
//!
//! - [`platform`]: traits a platform crate (rd-platform-macos, rd-platform-windows) implements.
//! - [`runtime`]: gateway control connection + per-session data connection, flow control glue.
//! - [`h264`]: avcC / codec-string / Annex-B helpers, SPS VUI fix-up.
//! - [`pattern`]: synthetic NV12 test pattern for the platforms' synthetic video sources.
//! - [`clock`]: the agent's monotonic microsecond clock (all wire timestamps use it).
//! - [`config`]: agent configuration file.
//! - [`selftest`]: the platform-neutral part of `rd-agent selftest`.

pub mod clock;
pub mod config;
pub mod h264;
pub mod pattern;
pub mod platform;
pub mod runtime;
pub mod selftest;

pub use platform::*;
