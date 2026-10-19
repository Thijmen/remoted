//! Host-specific pieces of the binary: the [`rd_agent_core::Platform`], token storage,
//! permissions, the login service (a LaunchAgent on macOS, an autostart entry on Windows) and
//! the selftest. Other targets compile with an "unsupported platform" error.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows)))]
pub use unsupported::*;

/// State of the login service that starts the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// The service definition file, if installed.
    pub installed: Option<PathBuf>,
    /// Loaded into the service manager.
    pub loaded: bool,
    /// e.g. "running", "not running".
    pub state: Option<String>,
    pub pid: Option<u32>,
}
