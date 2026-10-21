//! `rd-agent-svc.exe`: the LocalSystem service that runs the secure-desktop helper.
//!
//! Installed with `rd-agent secure install`; see `docs/setup-windows.md`.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(windows)]
    {
        let _log = init_log("secure-service.log");
        match rd_platform_windows::secure::service::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("rd-agent-svc: {e:#}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("rd-agent-svc is Windows only");
        ExitCode::FAILURE
    }
}

#[cfg(windows)]
fn init_log(name: &str) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = std::env::var_os("ProgramData")
        .map(PathBuf::from)?
        .join("rd")
        .join("agent")
        .join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let file = tracing_appender::rolling::daily(&dir, name);
    let (writer, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .init();
    Some(guard)
}
