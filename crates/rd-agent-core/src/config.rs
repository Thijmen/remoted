//! Agent configuration (`config.toml`).

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Gateway base URL for agents, e.g. `wss://rd-agent.example.com` or `ws://127.0.0.1:8081`.
    pub gateway_url: String,
    /// Machine id, e.g. "mbp". Must match the token's machine part.
    pub machine: String,
    /// Token `rd1.<machine>.<secret>`. `rd-agent setup` stores it here, in a file only the user
    /// can read. Older installs keep it in the Keychain; `RD_AGENT_TOKEN` overrides both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Defaults for new sessions (the viewer's `start` overrides).
    #[serde(default)]
    pub defaults: StreamDefaults,
    /// Self-updates (docs/updates.md). Written only when it differs from the default.
    #[serde(default, skip_serializing_if = "UpdateSettings::is_default")]
    pub update: UpdateSettings,
}

/// `[update]` in config.toml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// Look for new versions (1 minute after start, then every 6 hours) and report them to the
    /// gateway. When off, `rd-agent update` and the portal's Update button still work: both
    /// read the update manifest themselves first.
    #[serde(default = "yes")]
    pub check: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings { check: true }
    }
}

impl UpdateSettings {
    fn is_default(&self) -> bool {
        *self == UpdateSettings::default()
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamDefaults {
    pub max_kbps: u32,
    pub fps: u32,
}

impl Default for StreamDefaults {
    fn default() -> Self {
        StreamDefaults {
            max_kbps: 40_000,
            fps: 60,
        }
    }
}

impl AgentConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }

    /// Writes the file atomically and readable only by its owner (mode 0600 on Unix), since it
    /// may hold the agent token. Missing parent directories are created owner-only (0700);
    /// existing ones are left alone.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let name = path
            .file_name()
            .context("the config path has no file name")?
            .to_string_lossy();
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        write_private(&tmp, text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        // Replacing the file also replaces an older, more permissive one.
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Creates `path` (which must not exist) readable and writable by the owner only.
#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rd-agent-core-config-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_round_trips_and_is_private() {
        let dir = scratch("save");
        let path = dir.join("app").join("config.toml");
        let cfg = AgentConfig {
            gateway_url: "wss://gw".into(),
            machine: "mbp".into(),
            token: Some("rd1.mbp.secret".into()),
            defaults: StreamDefaults::default(),
            update: UpdateSettings::default(),
        };
        cfg.save(&path).unwrap();
        assert_eq!(AgentConfig::load(&path).unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&dir), 0o700);
            // An older world-readable file is replaced by a private one.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            cfg.save(&path).unwrap();
            assert_eq!(mode(&path), 0o600);
        }
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_is_optional() {
        let cfg: AgentConfig =
            toml::from_str("gateway_url = \"ws://x\"\nmachine = \"m\"\n").unwrap();
        assert_eq!(cfg.token, None);
        assert!(!toml::to_string(&cfg).unwrap().contains("token"));
    }

    #[test]
    fn update_checks_are_on_unless_turned_off() {
        let cfg: AgentConfig =
            toml::from_str("gateway_url = \"ws://x\"\nmachine = \"m\"\n").unwrap();
        assert!(cfg.update.check);
        // The default is not written out: setup leaves config files as they were.
        assert!(!toml::to_string(&cfg).unwrap().contains("update"));
        let cfg: AgentConfig =
            toml::from_str("gateway_url = \"ws://x\"\nmachine = \"m\"\n[update]\ncheck = false\n")
                .unwrap();
        assert!(!cfg.update.check);
        assert!(toml::to_string(&cfg).unwrap().contains("check = false"));
    }
}
