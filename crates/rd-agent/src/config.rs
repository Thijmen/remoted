//! Configuration file and token resolution.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use rd_agent_core::config::AgentConfig;

use crate::host;

/// Default token environment variable.
pub const TOKEN_ENV: &str = "RD_AGENT_TOKEN";

/// `--config` or the per-user default location.
pub fn path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p.to_owned()),
        None => host::config_path(),
    }
}

/// The config file (if any) with command-line overrides applied. Without a file, both
/// `gateway` and `machine` must be given.
pub fn load_for_run(
    explicit: Option<&Path>,
    gateway: Option<String>,
    machine: Option<String>,
) -> anyhow::Result<AgentConfig> {
    let path = path(explicit)?;
    let mut cfg = if path.is_file() {
        AgentConfig::load(&path).with_context(|| format!("reading {}", path.display()))?
    } else if explicit.is_some() {
        bail!("config file {} does not exist", path.display());
    } else {
        let (Some(gateway_url), Some(machine)) = (gateway.clone(), machine.clone()) else {
            bail!(
                "no config at {}; run `rd-agent setup` or pass --gateway and --machine",
                path.display()
            );
        };
        AgentConfig {
            gateway_url,
            machine,
            token: None,
            defaults: Default::default(),
            update: Default::default(),
        }
    };
    if let Some(gateway) = gateway {
        cfg.gateway_url = gateway;
    }
    if let Some(machine) = machine {
        cfg.machine = machine;
    }
    Ok(cfg)
}

/// Where the agent token came from (never the token itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// `--token-env VAR` or [`TOKEN_ENV`].
    Env(String),
    /// The config file (`rd-agent setup` puts it there).
    ConfigFile,
    /// The login Keychain: installs set up before the token moved into the config file.
    Keychain,
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::Env(var) => write!(f, "environment variable {var}"),
            TokenSource::ConfigFile => f.write_str("config file"),
            TokenSource::Keychain => f.write_str("Keychain"),
        }
    }
}

/// Token precedence: `--token-env VAR` (must be set), `RD_AGENT_TOKEN`, the config file, the
/// Keychain (read only when nothing else has a token, so a configured agent never touches it).
pub fn resolve_token(
    token_env: Option<&str>,
    cfg: &AgentConfig,
) -> anyhow::Result<(String, TokenSource)> {
    if let Some(var) = token_env {
        return non_empty(std::env::var(var).ok())
            .map(|t| (t, TokenSource::Env(var.to_owned())))
            .with_context(|| format!("--token-env {var}: the variable is not set"));
    }
    if let Some(token) = non_empty(std::env::var(TOKEN_ENV).ok()) {
        return Ok((token, TokenSource::Env(TOKEN_ENV.to_owned())));
    }
    if let Some(token) = non_empty(cfg.token.clone()) {
        return Ok((token, TokenSource::ConfigFile));
    }
    match non_empty(host::load_token()?) {
        Some(token) => Ok((token, TokenSource::Keychain)),
        None => bail!("no agent token: run `rd-agent setup`, or set {TOKEN_ENV}"),
    }
}

/// Owner-only permissions (0700) for the agent's own config directory, which holds the token.
/// Only for the per-user default location: a directory given with `--config` may be shared.
pub fn restrict_app_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The file's permission bits when others than the owner may read or write it (Unix).
pub fn too_open(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then_some(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The machine part of an `rd1.<machine>.<secret>` token.
pub fn token_machine(token: &str) -> Option<&str> {
    let rest = token.strip_prefix("rd1.")?;
    let (machine, secret) = rest.split_once('.')?;
    (!machine.is_empty() && !secret.is_empty()).then_some(machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape() {
        assert_eq!(token_machine("rd1.mbp.abc"), Some("mbp"));
        assert_eq!(token_machine("rd1.mbp.a.b"), Some("mbp"));
        assert_eq!(token_machine("rd1..abc"), None);
        assert_eq!(token_machine("rd1.mbp."), None);
        assert_eq!(token_machine("rd2.mbp.abc"), None);
        assert_eq!(token_machine("garbage"), None);
    }

    #[test]
    fn token_from_the_config_file() {
        // An inherited RD_AGENT_TOKEN would win; nothing to check then.
        if std::env::var_os(TOKEN_ENV).is_some() {
            return;
        }
        let cfg = AgentConfig {
            gateway_url: "wss://gw".into(),
            machine: "mbp".into(),
            token: Some(" rd1.mbp.secret\n".into()),
            defaults: Default::default(),
            update: Default::default(),
        };
        // Found before the Keychain is consulted (the test never touches the Keychain).
        let (token, source) = resolve_token(None, &cfg).unwrap();
        assert_eq!(token, "rd1.mbp.secret");
        assert_eq!(source, TokenSource::ConfigFile);
        assert_eq!(source.to_string(), "config file");
        // --token-env must name a variable that is set.
        let missing = format!("RD_AGENT_TEST_UNSET_{}", std::process::id());
        assert!(resolve_token(Some(&missing), &cfg).is_err());
    }

    #[test]
    fn private_files() {
        let dir = std::env::temp_dir().join(format!("rd-agent-perm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        let cfg = AgentConfig {
            gateway_url: "wss://gw".into(),
            machine: "mbp".into(),
            token: Some("rd1.mbp.secret".into()),
            defaults: Default::default(),
            update: Default::default(),
        };
        cfg.save(&file).unwrap();
        assert_eq!(too_open(&file), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(too_open(&file), Some(0o644));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            restrict_app_dir(&dir).unwrap();
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overrides_without_a_file() {
        let dir = std::env::temp_dir().join(format!("rd-agent-cfg-test-{}", std::process::id()));
        let missing = dir.join("missing.toml");
        // An explicit path must exist.
        assert!(load_for_run(Some(&missing), Some("ws://x".into()), Some("m".into())).is_err());

        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(
            &file,
            "gateway_url = \"wss://gw\"\nmachine = \"mbp\"\n[defaults]\nmax_kbps = 1000\nfps = 30\n",
        )
        .unwrap();
        let cfg = load_for_run(Some(&file), None, Some("other".into())).unwrap();
        assert_eq!(cfg.gateway_url, "wss://gw");
        assert_eq!(cfg.machine, "other");
        assert_eq!(cfg.defaults.fps, 30);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
