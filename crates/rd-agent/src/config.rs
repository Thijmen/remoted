//! Configuration file and token resolution.

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

/// Token precedence: `--token-env VAR` (must be set), `RD_AGENT_TOKEN`, the config file, the
/// Keychain.
pub fn resolve_token(token_env: Option<&str>, cfg: &AgentConfig) -> anyhow::Result<String> {
    if let Some(var) = token_env {
        return non_empty(std::env::var(var).ok())
            .with_context(|| format!("--token-env {var}: the variable is not set"));
    }
    if let Some(token) = non_empty(std::env::var(TOKEN_ENV).ok()) {
        return Ok(token);
    }
    if let Some(token) = non_empty(cfg.token.clone()) {
        return Ok(token);
    }
    match non_empty(host::load_token()?) {
        Some(token) => Ok(token),
        None => bail!("no agent token: run `rd-agent setup`, or set {TOKEN_ENV}"),
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
