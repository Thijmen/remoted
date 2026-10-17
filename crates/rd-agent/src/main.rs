//! rd-agent: the rd host agent.
//!
//! ```text
//! rd-agent run        [--config PATH] [--gateway URL] [--machine ID] [--source screen|synthetic] [--token-env VAR]
//! rd-agent setup      interactive, in person: token -> Keychain, config.toml, permission prompts, LaunchAgent
//! rd-agent install | uninstall     LaunchAgent management
//! rd-agent status     config, token, permissions, LaunchAgent state, gateway reachability
//! rd-agent selftest   --size 1728x1118 --fps 60 --seconds 10 [--bitrate-kbps 8000] [--record out.rdv] [--synthetic]
//! ```

mod config;
mod host;
mod logging;
mod setup;
mod status;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rd_agent_core::runtime::Runtime;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "rd-agent", version, about = "rd remote desktop host agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the gateway and serve viewer sessions (what the LaunchAgent runs).
    Run(RunArgs),
    /// Interactive first-time setup (run in person): token, config, permissions, LaunchAgent.
    Setup(SetupArgs),
    /// Install (or reinstall) and load the LaunchAgent.
    Install,
    /// Unload and remove the LaunchAgent.
    Uninstall,
    /// Show configuration, token, permissions, LaunchAgent and gateway status.
    Status(ConfigArg),
    /// Capture/encode benchmark with SPS/VUI assertions.
    Selftest(SelftestArgs),
}

#[derive(Args)]
struct ConfigArg {
    /// Config file [default: the per-user config location].
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    config: ConfigArg,
    /// Gateway URL for agents (overrides the config file), e.g. ws://127.0.0.1:8081.
    #[arg(long, value_name = "URL")]
    gateway: Option<String>,
    /// Machine id (overrides the config file).
    #[arg(long, value_name = "ID")]
    machine: Option<String>,
    /// Video source.
    #[arg(long, value_enum, default_value_t = Source::Screen)]
    source: Source,
    /// Read the token from this environment variable instead of RD_AGENT_TOKEN / Keychain.
    #[arg(long, value_name = "VAR")]
    token_env: Option<String>,
}

#[derive(Args)]
struct SetupArgs {
    #[command(flatten)]
    config: ConfigArg,
    /// Gateway URL (asked for when missing).
    #[arg(long, value_name = "URL")]
    gateway: Option<String>,
    /// Machine id (asked for when missing).
    #[arg(long, value_name = "ID")]
    machine: Option<String>,
    /// Do not install the LaunchAgent.
    #[arg(long)]
    no_launch_agent: bool,
}

#[derive(Args)]
struct SelftestArgs {
    /// Output size, WIDTHxHEIGHT (even numbers).
    #[arg(long, default_value = "1728x1118", value_parser = parse_size)]
    size: (u32, u32),
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 10)]
    seconds: u32,
    #[arg(long, default_value_t = 8000)]
    bitrate_kbps: u32,
    /// Write the stream as an .rdv recording.
    #[arg(long, value_name = "PATH")]
    record: Option<PathBuf>,
    /// Encode a moving test pattern instead of the screen (needs no permission).
    #[arg(long)]
    synthetic: bool,
    /// Encoder rate control: low-latency or standard [default: RD_VT_RATE_CONTROL or standard].
    #[arg(long, value_name = "MODE")]
    rate_control: Option<String>,
}

/// Where session video comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Source {
    /// Screen capture (needs the Screen Recording permission).
    Screen,
    /// A moving test pattern through the real encoder (needs no permission).
    Synthetic,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    let parse = |v: &str| v.parse::<u32>().map_err(|e| format!("{v}: {e}"));
    Ok((parse(w)?, parse(h)?))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => {
            let _logs = logging::init_agent();
            // Logged (stderr, file, os_log) rather than printed: usually nobody watches stderr.
            return match run(args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    error!("{e:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Setup(args) => {
            logging::init_cli();
            setup::run(&args)
        }
        Command::Install => {
            logging::init_cli();
            host::service::install()
        }
        Command::Uninstall => {
            logging::init_cli();
            host::service::uninstall()
        }
        Command::Status(args) => {
            logging::init_cli();
            status::run(args.config.as_deref())
        }
        Command::Selftest(args) => {
            logging::init_cli_verbose();
            return match host::selftest(&args) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("selftest error: {e:#}");
                    ExitCode::from(2)
                }
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rd-agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: RunArgs) -> anyhow::Result<()> {
    let mut cfg = config::load_for_run(args.config.config.as_deref(), args.gateway, args.machine)?;
    cfg.token = Some(config::resolve_token(args.token_env.as_deref(), &cfg)?);
    let platform = host::platform(args.source)?;
    info!(source = ?args.source, version = env!("CARGO_PKG_VERSION"), "rd-agent run");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("rd-agent")
        .enable_all()
        .build()?;
    runtime.block_on(Runtime::new(cfg, platform).run_until(shutdown_signal()))
}

/// SIGTERM (launchd stopping us) or SIGINT.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => info!("SIGTERM received; shutting down"),
                    _ = tokio::signal::ctrl_c() => info!("SIGINT received; shutting down"),
                }
            }
            Err(e) => {
                error!("cannot listen for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl-C received; shutting down");
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1728x1118"), Ok((1728, 1118)));
        assert!(parse_size("1728").is_err());
        assert!(parse_size("ax1").is_err());
    }

    #[test]
    fn run_flags() {
        let cli = Cli::try_parse_from([
            "rd-agent",
            "run",
            "--gateway",
            "ws://127.0.0.1:8081",
            "--machine",
            "mbp",
            "--source",
            "synthetic",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.source, Source::Synthetic);
        assert_eq!(args.machine.as_deref(), Some("mbp"));
    }
}
