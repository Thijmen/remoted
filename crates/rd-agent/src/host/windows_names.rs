//! Names and places shared by `rd-agent` and its background launcher `rd-agentw`
//! (`src/bin/rd-agentw.rs` includes this file; each binary uses part of it).

#![allow(dead_code)]

use std::path::PathBuf;

/// Directory under `%LOCALAPPDATA%` (not the roaming `%APPDATA%`: the token is per machine).
pub const APP_DIR: &str = "rd-agent";
/// Held by `rd-agent run` for its lifetime: one agent per session.
pub const RUN_MUTEX: &str = r"Local\dev.thijmen.rd.agent.run";
/// Signalled to stop `rd-agent run` gracefully (`install`, `uninstall`).
pub const STOP_EVENT: &str = r"Local\dev.thijmen.rd.agent.stop";
/// Held by the launcher: one launcher per session.
pub const LAUNCHER_MUTEX: &str = r"Local\dev.thijmen.rd.agent.launcher";
/// The launcher, next to `rd-agent.exe`.
pub const LAUNCHER_EXE: &str = "rd-agentw.exe";
pub const AGENT_EXE: &str = "rd-agent.exe";
/// Exit code of `rd-agent run` when another agent already runs in the session; the launcher
/// does not restart it then.
pub const EXIT_ALREADY_RUNNING: u8 = 3;

/// `%LOCALAPPDATA%\rd-agent`
pub fn app_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(APP_DIR))
}

/// `%LOCALAPPDATA%\rd-agent\logs`
pub fn log_dir() -> Option<PathBuf> {
    app_dir().map(|d| d.join("logs"))
}
