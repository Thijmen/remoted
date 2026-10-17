//! Placeholder for targets without a platform crate yet (Windows comes later).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use rd_agent_core::{Perms, Platform};

use crate::{SelftestArgs, Source};

const UNSUPPORTED: &str =
    "unsupported platform: rd-agent currently runs on macOS only (Windows support is planned)";

pub fn config_path() -> anyhow::Result<PathBuf> {
    bail!(UNSUPPORTED)
}

pub fn log_dir() -> anyhow::Result<PathBuf> {
    bail!(UNSUPPORTED)
}

pub fn platform(_source: Source) -> anyhow::Result<Arc<dyn Platform>> {
    bail!(UNSUPPORTED)
}

pub fn load_token() -> anyhow::Result<Option<String>> {
    bail!(UNSUPPORTED)
}

pub fn store_token(_token: &str) -> anyhow::Result<()> {
    bail!(UNSUPPORTED)
}

pub fn permissions() -> anyhow::Result<Perms> {
    bail!(UNSUPPORTED)
}

pub fn request_permissions() -> anyhow::Result<Perms> {
    bail!(UNSUPPORTED)
}

pub fn selftest(_args: &SelftestArgs) -> anyhow::Result<bool> {
    bail!(UNSUPPORTED)
}

pub mod service {
    use anyhow::bail;

    use super::UNSUPPORTED;
    use crate::host::ServiceStatus;

    pub fn install() -> anyhow::Result<()> {
        bail!(UNSUPPORTED)
    }

    pub fn uninstall() -> anyhow::Result<()> {
        bail!(UNSUPPORTED)
    }

    pub fn status() -> anyhow::Result<ServiceStatus> {
        bail!(UNSUPPORTED)
    }
}
