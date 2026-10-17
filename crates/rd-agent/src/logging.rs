//! Logging: human-readable lines on stderr, plus for `run` the unified log (macOS `os_log`,
//! subsystem `dev.thijmen.rd.agent`) and a daily rolling file (7 kept).
//!
//! Under launchd stderr goes to an unrotated file (`launchd.log`), so when stderr is not a
//! terminal it only gets warnings and errors; the rolling file and os_log get everything.

use std::io::IsTerminal as _;

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _, fmt};

use crate::host;

const LOG_FILES_KEPT: usize = 7;

fn filter(default: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
}

/// Interactive commands: warnings and errors only (they print their own output).
pub fn init_cli() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter("warn"))
        .with_writer(std::io::stderr)
        .try_init();
}

/// `selftest`: progress at info level.
pub fn init_cli_verbose() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter("info"))
        .with_writer(std::io::stderr)
        .try_init();
}

/// `run`: stderr + os_log + rolling file. Keep the returned guard alive: it flushes the file
/// writer on drop.
pub fn init_agent() -> Option<WorkerGuard> {
    let (file, guard, file_error) = match file_appender() {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer().with_ansi(false).with_writer(writer);
            (Some(layer), Some(guard), None)
        }
        Err(e) => (None, None, Some(e)),
    };
    let stderr_level = if std::io::stderr().is_terminal() {
        LevelFilter::TRACE
    } else {
        LevelFilter::WARN
    };
    let registry = tracing_subscriber::registry()
        .with(filter("info"))
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_level),
        )
        .with(file);
    #[cfg(target_os = "macos")]
    let registry = registry.with(tracing_oslog::OsLogger::new(host::LOG_SUBSYSTEM, "agent"));
    if registry.try_init().is_ok() {
        install_panic_hook();
    }
    if let Some(e) = file_error {
        tracing::warn!("file logging disabled: {e:#}");
    }
    guard
}

fn file_appender() -> anyhow::Result<RollingFileAppender> {
    let dir = host::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("rd-agent")
        .filename_suffix("log")
        .max_log_files(LOG_FILES_KEPT)
        .build(&dir)?)
}

/// Panics also go to the log files (under launchd nobody reads stderr).
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default(info);
    }));
}
