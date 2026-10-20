//! rd-agent: the rd host agent (macOS and Windows).
//!
//! ```text
//! rd-agent run        [--config PATH] [--gateway URL] [--machine ID] [--source screen|synthetic] [--token-env VAR]
//! rd-agent setup      interactive, in person: config.toml (with the token, readable only by you), permissions, login service
//! rd-agent install | uninstall     login service management (macOS: LaunchAgent; Windows: autostart + rd-agentw.exe)
//! rd-agent status     config, token, permissions, login service state, gateway reachability
//! rd-agent update     [--check] install the newest version now (docs/updates.md)
//! rd-agent selftest   --size 1728x1118 --fps 60 --seconds 10 [--bitrate-kbps 8000] [--record out.rdv] [--synthetic]
//! ```

mod config;
mod host;
mod logging;
mod setup;
mod status;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rd_agent_core::runtime::{Runtime, update_link};
use rd_proto::control::UpdateStatus;
use tracing::{error, info};

/// This build's version: reported to the gateway and viewers, and what updates compare
/// against. Debug builds can be given another at build time (`RD_AGENT_TEST_VERSION=X.Y.Z
/// cargo build`), to test updates between two builds of the same source.
pub const VERSION: &str = match option_env!("RD_AGENT_TEST_VERSION") {
    Some(v) if cfg!(debug_assertions) => v,
    _ => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "rd-agent", version = VERSION, about = "rd remote desktop host agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the gateway and serve viewer sessions (what the login service runs).
    Run(RunArgs),
    /// Interactive first-time setup (run in person): token, config, permissions, login service.
    Setup(SetupArgs),
    /// Install (or reinstall) and start the login service (macOS: LaunchAgent; Windows:
    /// autostart entry).
    Install,
    /// Stop and remove the login service.
    Uninstall,
    /// Show configuration, token, permissions, login service and gateway status.
    Status(ConfigArg),
    /// Install the newest version now, as the portal's Update button does (only for agents
    /// installed with the installer / as RD Agent.app with its LaunchAgent).
    Update(UpdateArgs),
    /// Capture/encode benchmark with SPS/VUI assertions.
    Selftest(SelftestArgs),
    /// Internal: report (and with --request, ask for) this app's permissions. `setup` and
    /// `status` start it through LaunchServices so macOS attributes it to RD Agent.app rather
    /// than to the terminal.
    #[command(hide = true)]
    Permissions(PermissionsArgs),
    /// Internal: installs an update and rolls it back if the new version does not come back.
    /// The agent starts it from a copy of itself.
    #[command(hide = true)]
    UpdateHelper(UpdateHelperArgs),
}

#[derive(Args)]
struct UpdateArgs {
    #[command(flatten)]
    config: ConfigArg,
    /// Only show what is available (and the last update's outcome).
    #[arg(long)]
    check: bool,
}

#[derive(Args)]
struct UpdateHelperArgs {
    /// The plan the agent wrote.
    #[arg(long, value_name = "PATH")]
    plan: PathBuf,
}

#[derive(Args)]
struct PermissionsArgs {
    /// Show the standard system prompts for missing permissions.
    #[arg(long)]
    request: bool,
    /// With --request: ask for Screen Recording only (view-only use, e.g. testing a viewer on
    /// the same Mac, where injected input would loop back into the viewer).
    #[arg(long, requires = "request")]
    screen_only: bool,
    /// Write the result here as JSON.
    #[arg(long, value_name = "PATH")]
    report: PathBuf,
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
    /// Read the token from this environment variable instead of RD_AGENT_TOKEN / the config
    /// file / the Keychain.
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
    /// Do not install the login service (LaunchAgent / autostart).
    #[arg(long, alias = "no-autostart")]
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
    /// macOS: encoder rate control, low-latency or standard [default: RD_VT_RATE_CONTROL or
    /// standard].
    #[arg(long, value_name = "MODE")]
    rate_control: Option<String>,
    /// Windows: encoder, auto (the GPU's, else software), hardware or software [default:
    /// RD_MF_ENCODER or auto].
    #[arg(long, value_name = "ENCODER")]
    encoder: Option<String>,
}

/// Where session video comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Source {
    /// Screen capture (on macOS it needs the Screen Recording permission).
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
                    #[cfg(windows)]
                    if e.is::<host::AlreadyRunning>() {
                        return ExitCode::from(host::names::EXIT_ALREADY_RUNNING);
                    }
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
        Command::Update(args) => {
            logging::init_cli();
            config::path(args.config.config.as_deref())
                .and_then(|path| update::cli(&path, args.check))
        }
        Command::UpdateHelper(args) => update::helper::main(&args.plan),
        Command::Permissions(args) => {
            logging::init_cli();
            host::report_permissions(args.request, args.screen_only, &args.report)
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
    // One agent per session; `install` / `uninstall` stop it through the guard's event.
    #[cfg(windows)]
    let guard = host::RunGuard::acquire()?;
    let config_path = config::path(args.config.config.as_deref())?;
    let mut cfg = config::load_for_run(args.config.config.as_deref(), args.gateway, args.machine)?;
    let (token, token_source) = config::resolve_token(args.token_env.as_deref(), &cfg)?;
    cfg.token = Some(token);
    let platform = host::platform(args.source)?;
    info!(
        source = ?args.source,
        token_from = %token_source,
        version = VERSION,
        "rd-agent run"
    );
    let updater = Arc::new(update::updater(&config_path)?);
    let check = cfg.update.check;
    let (link, updates) = update_link(UpdateStatus::default());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("rd-agent")
        .enable_all()
        .build()?;
    #[cfg(windows)]
    let stopped = guard.stopped();
    #[cfg(not(windows))]
    let stopped = std::future::pending::<()>();
    runtime.block_on(async move {
        tokio::spawn(update::service::run(updates, updater, check));
        Runtime::new(cfg, platform)
            .version(VERSION)
            .updates(link)
            .run_until(shutdown_signal(stopped))
            .await
    })
}

/// SIGTERM (launchd stopping us), SIGINT / Ctrl-C, or `stopped` (Windows: the stop event of
/// `rd-agent install` / `uninstall`; never on macOS).
async fn shutdown_signal(stopped: impl std::future::Future<Output = ()>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => info!("SIGTERM received; shutting down"),
                    _ = tokio::signal::ctrl_c() => info!("SIGINT received; shutting down"),
                    () = stopped => info!("asked to stop; shutting down"),
                }
            }
            Err(e) => {
                error!("cannot listen for SIGTERM: {e}");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    () = stopped => {}
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("Ctrl-C received; shutting down"),
            () = stopped => info!("asked to stop; shutting down"),
        }
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
