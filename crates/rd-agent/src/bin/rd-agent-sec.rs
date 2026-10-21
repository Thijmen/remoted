//! `rd-agent-sec.exe`: the SYSTEM helper that captures and controls the Windows secure
//! desktop. Launched by `rd-agent-svc.exe`; never run by hand except for debugging
//! (`rd-agent-sec --pipe <name>` as SYSTEM).

#[cfg(windows)]
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(windows)]
    {
        let _log = init_log("secure-helper.log");
        let mut pipe = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--pipe" {
                pipe = args.next();
            }
        }
        let Some(pipe) = pipe else {
            tracing::error!("usage: rd-agent-sec --pipe <name>");
            return ExitCode::FAILURE;
        };
        match rd_platform_windows::secure::helper::run(&pipe) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("rd-agent-sec: {e:#}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("rd-agent-sec is Windows only");
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
