//! Self-updates (docs/updates.md, "rd agent").
//!
//! ```text
//! service.rs    `rd-agent run`: periodic checks, update_status, the registered marker, and
//!               installing on the gateway's `update`
//! cli.rs        `rd-agent update [--check]`
//! prepare.rs    read the manifest, download, check the signature and the new version, copy
//!               the running version aside, write the plan, start the helper
//! helper.rs     `rd-agent update-helper`: install, wait for the new version, roll back
//! target.rs     can this agent install updates (installed the normal way)? where?
//! files.rs      the update directory shared by old and new versions
//! manifest.rs   agent.json: the newest version, its file, the notes since ours
//! fetch.rs      HTTPS downloads (ureq, rustls + ring)
//! signature.rs  the built-in minisign key
//! version.rs    X.Y.Z[-pre] ordering
//! ```

mod cli;
mod fetch;
mod files;
pub mod helper;
mod manifest;
mod prepare;
pub mod service;
mod signature;
mod target;
mod version;

use std::path::Path;

pub use prepare::Updater;

use crate::host;
use files::UpdateDir;

/// The updater for the agent whose config file is `config` (the update directory lives next
/// to it).
pub fn updater(config: &Path) -> anyhow::Result<Updater> {
    Ok(Updater::new(
        crate::VERSION,
        UpdateDir::for_config(config),
        host::log_dir()?,
    ))
}

/// `rd-agent update [--check]`.
pub fn cli(config: &Path, check_only: bool) -> anyhow::Result<()> {
    cli::run(&updater(config)?, check_only)
}
