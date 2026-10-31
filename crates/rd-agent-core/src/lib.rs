//! Platform-neutral rd agent.
//!
//! - [`platform`]: traits a platform crate (rd-platform-macos, rd-platform-windows) implements.
//! - [`runtime`]: gateway control connection + per-session data connection, flow control glue.
//! - [`h264`]: avcC / codec-string / Annex-B helpers, SPS VUI fix-up.
//! - [`pattern`]: synthetic NV12 test pattern for the platforms' synthetic video sources.
//! - [`nv12`]: converting BGRA frames to NV12 for capture paths without a GPU.
//! - [`clock`]: the agent's monotonic microsecond clock (all wire timestamps use it).
//! - [`config`]: agent configuration file.
//! - [`headless`]: platforms without a screen, for agents that serve only the terminal and
//!   files (Linux servers, `[terminal] only = true`).
//! - `pty` (Unix): pseudo terminals for the terminal's shells.
//! - [`selftest`]: the platform-neutral part of `rd-agent selftest`.

pub mod clock;
pub mod config;
pub mod h264;
pub mod headless;
pub mod nv12;
pub mod pattern;
pub mod platform;
#[cfg(unix)]
pub mod pty;
pub mod runtime;
pub mod selftest;

pub use platform::*;
