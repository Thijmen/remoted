//! Command line / environment configuration (see `docs/gateway.md`).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::chaos::Chaos;

#[derive(Debug, Parser)]
#[command(
    name = "rd-gateway",
    version,
    about = "rd relay, portal and network probe"
)]
pub struct Cli {
    /// Development mode: no auth, loopback-only defaults, human-readable logs, RD_CHAOS honoured.
    #[arg(long)]
    pub dev: bool,
    /// Viewer listener (portal, viewer, probe). Default 0.0.0.0:8080 (127.0.0.1:8080 with --dev).
    #[arg(long, env = "RD_VIEWER_ADDR")]
    pub viewer_addr: Option<SocketAddr>,
    /// Agent listener. Default 0.0.0.0:8081 (127.0.0.1:8081 with --dev).
    #[arg(long, env = "RD_AGENT_ADDR")]
    pub agent_addr: Option<SocketAddr>,
    /// Health/metrics listener. Default 0.0.0.0:9090 (127.0.0.1:9090 with --dev).
    #[arg(long, env = "RD_METRICS_ADDR")]
    pub metrics_addr: Option<SocketAddr>,
    /// Exact `Origin` required on viewer WebSocket upgrades, e.g. https://rd.example.com.
    #[arg(long, env = "RD_PUBLIC_ORIGIN")]
    pub public_origin: Option<String>,
    /// Comma-separated allowlist for `X-authentik-email` (case-insensitive).
    #[arg(long, env = "RD_ALLOWED_EMAILS", value_delimiter = ',')]
    pub allowed_emails: Vec<String>,
    /// Optional comma-separated allowlist for `X-authentik-uid`. When set, a viewer must match
    /// both lists: unlike the email, Authentik's uid is unique and cannot be edited by the user.
    #[arg(long, env = "RD_ALLOWED_UIDS", value_delimiter = ',')]
    pub allowed_uids: Vec<String>,
    /// Directory with one file per machine holding the hex SHA-256 of its agent token.
    #[arg(long, env = "RD_AGENT_TOKENS_DIR", default_value = "/etc/rd/agents")]
    pub agent_tokens_dir: PathBuf,
    /// JSON list of known machines (shown in the portal even while offline).
    #[arg(long, env = "RD_MACHINES_FILE")]
    pub machines_file: Option<PathBuf>,
    /// End a session after this many seconds without viewer input.
    #[arg(long, env = "RD_SESSION_IDLE_TIMEOUT", default_value_t = 3600)]
    pub session_idle_timeout: u64,
    /// Failure injection on agent→viewer traffic, e.g. `bw_kbps=5000,delay_ms=50` (--dev only).
    #[arg(long, env = "RD_CHAOS")]
    pub chaos: Option<String>,
    /// Serve the viewer build from this directory instead of the embedded copy.
    #[arg(long, env = "RD_ASSETS_DIR")]
    pub assets_dir: Option<PathBuf>,
    /// Writable directory for the gateway's own state: the devices added in the portal
    /// (`devices.json`). Without it *Add device* is off (with --dev devices are kept in memory).
    #[arg(long, env = "RD_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    /// Where agents connect, e.g. wss://rd-agent.example.com; goes into setup codes. Default
    /// with --dev: ws://<agent listener>.
    #[arg(long, env = "RD_AGENT_PUBLIC_URL")]
    pub agent_public_url: Option<String>,
}

/// One entry of `RD_MACHINES_FILE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub machine: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub recovery: Vec<RecoveryLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryLink {
    pub label: String,
    pub url: String,
}

/// Keepalive and session timers. Production values come from [`Timeouts::default`]; tests
/// shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// WebSocket Ping interval on every leg.
    pub ping_interval: Duration,
    /// Close a leg when no Pong arrived for this long.
    pub pong_timeout: Duration,
    /// How long the agent has to dial the session socket after `session_request`.
    pub session_request: Duration,
    /// End a session after this long without viewer input.
    pub idle: Duration,
    /// How long a new control socket may take to send `register`.
    pub register: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            ping_interval: Duration::from_secs(15),
            pong_timeout: Duration::from_secs(45),
            session_request: Duration::from_secs(10),
            idle: Duration::from_secs(3600),
            register: Duration::from_secs(10),
        }
    }
}

/// Validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub dev: bool,
    pub viewer_addr: SocketAddr,
    pub agent_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    /// Lower-cased origin without trailing slash. `None` only in dev mode.
    pub public_origin: Option<String>,
    /// Lower-cased emails.
    pub allowed_emails: HashSet<String>,
    /// Authentik uids; empty = not checked.
    pub allowed_uids: HashSet<String>,
    pub agent_tokens_dir: PathBuf,
    pub machines: Vec<MachineConfig>,
    pub assets_dir: Option<PathBuf>,
    /// Device store directory (`RD_STATE_DIR`).
    pub state_dir: Option<PathBuf>,
    /// `ws://` or `wss://` URL agents connect to (`RD_AGENT_PUBLIC_URL`), without trailing slash.
    pub agent_public_url: Option<String>,
    /// Only ever `Some` in dev mode.
    pub chaos: Option<Chaos>,
    pub timeouts: Timeouts,
}

impl Config {
    /// Dev-mode configuration on ephemeral loopback ports (used by tests and as a base).
    pub fn dev_ephemeral() -> Self {
        let lo = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        Config {
            dev: true,
            viewer_addr: lo(0),
            agent_addr: lo(0),
            metrics_addr: lo(0),
            public_origin: None,
            allowed_emails: HashSet::new(),
            allowed_uids: HashSet::new(),
            agent_tokens_dir: PathBuf::from("/etc/rd/agents"),
            machines: Vec::new(),
            assets_dir: None,
            state_dir: None,
            agent_public_url: None,
            chaos: None,
            timeouts: Timeouts::default(),
        }
    }

    pub fn from_cli(cli: Cli) -> anyhow::Result<Self> {
        let host = if cli.dev {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::UNSPECIFIED
        };
        let addr =
            |given: Option<SocketAddr>, port| given.unwrap_or(SocketAddr::from((host, port)));

        let allowed_emails: HashSet<String> = cli
            .allowed_emails
            .iter()
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let allowed_uids: HashSet<String> = cli
            .allowed_uids
            .iter()
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty())
            .collect();
        let public_origin = cli
            .public_origin
            .as_deref()
            .map(normalize_origin)
            .filter(|o| !o.is_empty());
        if !cli.dev {
            if public_origin.is_none() {
                bail!("RD_PUBLIC_ORIGIN is required (or run with --dev)");
            }
            if allowed_emails.is_empty() {
                bail!("RD_ALLOWED_EMAILS is required (or run with --dev)");
            }
        }

        let agent_public_url = cli
            .agent_public_url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_owned())
            .filter(|u| !u.is_empty());
        if let Some(url) = &agent_public_url {
            let ok = url
                .split_once("://")
                .is_some_and(|(scheme, host)| matches!(scheme, "ws" | "wss") && !host.is_empty());
            if !ok {
                bail!("RD_AGENT_PUBLIC_URL must be a ws:// or wss:// URL, got {url:?}");
            }
        }

        let machines = match &cli.machines_file {
            Some(path) => load_machines(path)?,
            None => Vec::new(),
        };

        let chaos = match cli.chaos.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(spec) if cli.dev => Some(spec.parse::<Chaos>().context("invalid RD_CHAOS")?),
            Some(_) => {
                tracing::warn!("RD_CHAOS is ignored without --dev");
                None
            }
            None => None,
        };

        Ok(Config {
            dev: cli.dev,
            viewer_addr: addr(cli.viewer_addr, 8080),
            agent_addr: addr(cli.agent_addr, 8081),
            metrics_addr: addr(cli.metrics_addr, 9090),
            public_origin,
            allowed_emails,
            allowed_uids,
            agent_tokens_dir: cli.agent_tokens_dir,
            machines,
            assets_dir: cli.assets_dir,
            state_dir: cli.state_dir,
            agent_public_url,
            chaos,
            timeouts: Timeouts {
                idle: Duration::from_secs(cli.session_idle_timeout.max(1)),
                ..Timeouts::default()
            },
        })
    }
}

pub fn normalize_origin(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Machine ids appear in URLs, token files and tokens (`rd1.<machine>.<secret>`), so they are
/// restricted to `[A-Za-z0-9_-]{1,64}` (no dots, no path separators).
pub fn valid_machine_name(name: &str) -> bool {
    rd_proto::setup_code::valid_machine_id(name)
}

fn load_machines(path: &std::path::Path) -> anyhow::Result<Vec<MachineConfig>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading RD_MACHINES_FILE {}", path.display()))?;
    let machines: Vec<MachineConfig> = serde_json::from_str(&text)
        .with_context(|| format!("parsing RD_MACHINES_FILE {}", path.display()))?;
    let mut seen = HashSet::new();
    for m in &machines {
        if !valid_machine_name(&m.machine) {
            bail!("RD_MACHINES_FILE: invalid machine id {:?}", m.machine);
        }
        if !seen.insert(m.machine.as_str()) {
            bail!("RD_MACHINES_FILE: duplicate machine id {:?}", m.machine);
        }
    }
    Ok(machines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("rd-gateway").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn production_requires_origin_and_allowlist() {
        assert!(Config::from_cli(cli(&[])).is_err());
        assert!(Config::from_cli(cli(&["--public-origin", "https://x"])).is_err());
        let c = Config::from_cli(cli(&[
            "--public-origin",
            "https://RD.example/",
            "--allowed-emails",
            "A@x.com, b@y.com,",
        ]))
        .unwrap();
        assert_eq!(c.public_origin.as_deref(), Some("https://rd.example"));
        assert!(c.allowed_emails.contains("a@x.com"));
        assert!(c.allowed_emails.contains("b@y.com"));
        assert_eq!(c.allowed_emails.len(), 2);
        assert_eq!(c.viewer_addr, "0.0.0.0:8080".parse().unwrap());
        assert!(c.chaos.is_none());
    }

    #[test]
    fn dev_defaults_to_loopback_and_honours_chaos() {
        let c = Config::from_cli(cli(&["--dev", "--chaos", "bw_kbps=5000,delay_ms=50"])).unwrap();
        assert_eq!(c.viewer_addr, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(c.agent_addr, "127.0.0.1:8081".parse().unwrap());
        assert_eq!(c.metrics_addr, "127.0.0.1:9090".parse().unwrap());
        assert!(c.chaos.is_some());
        let c = Config::from_cli(cli(&[
            "--public-origin",
            "https://x",
            "--allowed-emails",
            "a@x",
            "--chaos",
            "delay_ms=5",
        ]))
        .unwrap();
        assert!(c.chaos.is_none(), "chaos must be dev-only");
    }

    #[test]
    fn machine_names() {
        assert!(valid_machine_name("mbp"));
        assert!(valid_machine_name("desktop-pc_1"));
        assert!(!valid_machine_name(""));
        assert!(!valid_machine_name("../etc"));
        assert!(!valid_machine_name("a.b"));
        assert!(!valid_machine_name(&"x".repeat(65)));
    }
}
