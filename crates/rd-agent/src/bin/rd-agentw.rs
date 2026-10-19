//! rd-agentw: the Windows background launcher of rd-agent (what the autostart runs).
//!
//! `rd-agent.exe` is a console program, so starting it at login would open a console window.
//! This GUI-subsystem launcher starts `rd-agent.exe run` (next to itself; extra arguments are
//! passed instead of `run`) without a window, appends the agent's stderr to
//! `%LOCALAPPDATA%\rd-agent\logs\agent-stderr.log`, and restarts it after a failure, like
//! launchd's KeepAlive with a 10 s throttle on macOS. It ends when the agent exits cleanly
//! (stopped by `rd-agent install` / `uninstall`) or finds another agent in the session.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "../host/windows_names.rs"]
mod names;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    launcher::run()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("rd-agentw is the Windows background launcher; run rd-agent directly");
    std::process::ExitCode::from(2)
}

#[cfg(windows)]
mod launcher {
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, ExitCode, Stdio};
    use std::time::{Duration, Instant, SystemTime};

    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;

    use super::names;

    /// `CREATE_NO_WINDOW`: the agent gets a console (it is a console program) but no window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// Wait this long before restarting a failed agent (launchd's ThrottleInterval).
    const THROTTLE: Duration = Duration::from_secs(10);

    pub fn run() -> ExitCode {
        // One launcher per session; the mutex is released when this process exits.
        // SAFETY: named mutex; the handle is intentionally kept until exit.
        let mutex = unsafe { CreateMutexW(None, true, &HSTRING::from(names::LAUNCHER_MUTEX)) };
        if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return ExitCode::SUCCESS;
        }
        let Some(log_dir) = names::log_dir() else {
            return ExitCode::FAILURE;
        };
        let _ = std::fs::create_dir_all(&log_dir);
        let log_path = log_dir.join("agent-stderr.log");
        let note = |msg: &str| {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&log_path) {
                let secs = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let _ = writeln!(f, "[rd-agentw {secs}] {msg}");
            }
        };
        let Ok(exe) = std::env::current_exe() else {
            return ExitCode::FAILURE;
        };
        let agent = exe.with_file_name(names::AGENT_EXE);
        let args: Vec<String> = std::env::args().skip(1).collect();
        let args = if args.is_empty() {
            vec!["run".to_owned()]
        } else {
            args
        };
        loop {
            let stderr = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_or_else(|_| Stdio::null(), Stdio::from);
            let started = Instant::now();
            let status = Command::new(&agent)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr)
                .status();
            match status {
                Ok(s) if s.success() => return ExitCode::SUCCESS,
                Ok(s) if s.code() == Some(i32::from(names::EXIT_ALREADY_RUNNING)) => {
                    note("another rd-agent is already running in this session; exiting");
                    return ExitCode::SUCCESS;
                }
                Ok(s) => note(&format!(
                    "{} exited with {s} after {} s; restarting in {} s",
                    agent.display(),
                    started.elapsed().as_secs(),
                    THROTTLE.as_secs()
                )),
                Err(e) => {
                    note(&format!("cannot start {}: {e}", agent.display()));
                    return ExitCode::FAILURE;
                }
            }
            std::thread::sleep(THROTTLE);
        }
    }
}
