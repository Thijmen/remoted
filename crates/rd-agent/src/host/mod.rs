//! Host-specific pieces of the binary: the [`rd_agent_core::Platform`], token storage,
//! permissions, the login service (LaunchAgent) and the selftest. macOS is implemented; other
//! targets compile with an "unsupported platform" error until their platform crate exists.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(target_os = "macos"))]
mod unsupported;
#[cfg(not(target_os = "macos"))]
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
