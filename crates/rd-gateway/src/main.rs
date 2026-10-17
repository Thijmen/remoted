//! rd-gateway binary: parse config, set up logging, serve until SIGTERM/SIGINT.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use rd_gateway::{Cli, Config, Gateway};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.dev);
    let cfg = match Config::from_cli(cli) {
        Ok(cfg) => cfg,
        Err(err) => {
            error!("configuration error: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.dev {
        warn!("dev mode: viewer and agent authentication are DISABLED");
        for (what, addr) in [
            ("viewer", cfg.viewer_addr),
            ("agent", cfg.agent_addr),
            ("metrics", cfg.metrics_addr),
        ] {
            if !addr.ip().is_loopback() {
                warn!(%addr, "dev mode {what} listener is reachable from the network without auth");
            }
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            error!("failed to start runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let gateway = Gateway::bind(cfg).await?;
        gateway.run(shutdown_signal()).await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// JSON lines in production, human-readable output with `--dev`. `RUST_LOG` defaults to `info`.
fn init_logging(dev: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if dev {
        builder
            .compact()
            .with_ansi(std::io::stdout().is_terminal())
            .init();
    } else {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .init();
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!("cannot listen for SIGINT: {err}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                warn!("cannot listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => info!("SIGINT received"),
        () = terminate => info!("SIGTERM received"),
    }
}
