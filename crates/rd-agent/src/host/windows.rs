//! Windows: [`WinPlatform`], file locations, the single-instance guard and stop event of
//! `rd-agent run`, the per-user autostart (Run key + `rd-agentw.exe`), selftest.
//!
//! Nothing needs to be granted on Windows: screen capture and input injection work for any
//! process in the user's interactive session.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::{Perms, Platform};
use rd_platform_windows::{EncoderPreference, VideoSource, WinPlatform, power, session};
use tracing::info;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
    OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::core::HSTRING;

#[path = "windows_names.rs"]
pub mod names;

use crate::{SelftestArgs, Source};

/// `%LOCALAPPDATA%\rd-agent\config.toml`
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(names::app_dir()
        .context("LOCALAPPDATA is not set")?
        .join("config.toml"))
}

/// `%LOCALAPPDATA%\rd-agent\logs`, or `RD_AGENT_LOG_DIR`.
pub fn log_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = super::log_dir_override() {
        return Ok(dir);
    }
    names::log_dir().context("LOCALAPPDATA is not set")
}

pub fn platform(source: Source) -> anyhow::Result<Arc<dyn Platform>> {
    let video = match source {
        Source::Screen => VideoSource::Screen,
        Source::Synthetic => VideoSource::Synthetic,
    };
    let platform: Arc<dyn Platform> =
        WinPlatform::with_options(video, EncoderPreference::from_env())?;
    Ok(platform)
}

/// There is no older token store on Windows: the token lives in the config file.
pub fn load_token() -> anyhow::Result<Option<String>> {
    Ok(None)
}

pub fn app_permissions(_request: bool) -> anyhow::Result<Perms> {
    Ok(session::permissions())
}

/// `rd-agent permissions` (internal; used on macOS): reports the (always granted) permissions.
pub fn report_permissions(
    _request: bool,
    _screen_only: bool,
    report: &std::path::Path,
) -> anyhow::Result<()> {
    let tmp = report.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&session::permissions())?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, report).with_context(|| format!("writing {}", report.display()))
}

/// Blocks for the duration of the test.
pub fn selftest(args: &SelftestArgs) -> anyhow::Result<bool> {
    ensure!(
        args.rate_control.is_none(),
        "--rate-control is a macOS (VideoToolbox) option; on Windows choose the encoder with \
         --encoder auto|hardware|software"
    );
    let preference = match &args.encoder {
        Some(e) => e.parse()?,
        None => EncoderPreference::from_env(),
    };
    let video = if args.synthetic {
        VideoSource::Synthetic
    } else {
        VideoSource::Screen
    };
    let platform = WinPlatform::with_options(video, preference)?;
    // A dark display delivers no frames.
    let _awake = power::keep_display_awake("rd-agent selftest");
    power::declare_user_activity();
    let opts = rd_agent_core::selftest::SelftestOptions {
        width: args.size.0,
        height: args.size.1,
        fps: args.fps,
        seconds: args.seconds,
        bitrate_kbps: args.bitrate_kbps,
        record: args.record.clone(),
        synthetic: args.synthetic,
        encoder: format!("{} encoder", preference.as_str()),
    };
    rd_agent_core::selftest::run(platform.video_pipeline()?, &opts, resident_bytes)
}

/// Working set of this process.
fn resident_bytes() -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a writable PROCESS_MEMORY_COUNTERS of the size given.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) }.ok()?;
    Some(counters.WorkingSetSize as u64)
}

/// Another `rd-agent run` holds the session's agent mutex.
#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "another rd-agent is already running in this session (stop it with \
             `rd-agent uninstall`, or `rd-agent install` to restart it)",
        )
    }
}

impl std::error::Error for AlreadyRunning {}

/// Held by `rd-agent run`: the session's agent mutex and the stop event.
pub struct RunGuard {
    mutex: HANDLE,
    stop: HANDLE,
}

// SAFETY: kernel object handles may be used and closed from any thread.
unsafe impl Send for RunGuard {}
unsafe impl Sync for RunGuard {}

impl RunGuard {
    /// Fails with [`AlreadyRunning`] when another agent runs in this session.
    pub fn acquire() -> anyhow::Result<Self> {
        // SAFETY: named kernel objects; the handles are closed in Drop.
        unsafe {
            let mutex = CreateMutexW(None, true, &HSTRING::from(names::RUN_MUTEX))
                .context("CreateMutexW")?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                return Err(AlreadyRunning.into());
            }
            let stop = match CreateEventW(None, true, false, &HSTRING::from(names::STOP_EVENT)) {
                Ok(h) => h,
                Err(e) => {
                    let _ = CloseHandle(mutex);
                    return Err(e).context("CreateEventW");
                }
            };
            Ok(RunGuard { mutex, stop })
        }
    }

    /// Resolves when `rd-agent install` / `uninstall` asks this agent to stop.
    pub fn stopped(&self) -> impl std::future::Future<Output = ()> + use<> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stop = self.stop.0 as usize;
        // A plain thread, not spawn_blocking: the runtime must not wait for it on shutdown.
        let spawned = std::thread::Builder::new()
            .name("rd-stop-event".into())
            .spawn(move || {
                // SAFETY: the event handle lives as long as the guard, which outlives `run`.
                if unsafe { WaitForSingleObject(HANDLE(stop as *mut _), INFINITE) } == WAIT_OBJECT_0
                {
                    let _ = tx.send(());
                }
            });
        async move {
            if spawned.is_err() || rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // SAFETY: handles created in `acquire`, closed once.
        unsafe {
            let _ = CloseHandle(self.stop);
            let _ = CloseHandle(self.mutex);
        }
    }
}

/// Whether an agent runs in this session.
fn agent_running() -> bool {
    // SAFETY: opening a named mutex only to see whether it exists.
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            &HSTRING::from(names::RUN_MUTEX),
        )
    } {
        Ok(h) => {
            // SAFETY: opened above.
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

/// Asks a running agent to stop and waits up to `wait` for it to exit. Ok(false) if none ran.
fn stop_agent(wait: std::time::Duration) -> anyhow::Result<bool> {
    // SAFETY: named event; signalled and closed.
    let event =
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(names::STOP_EVENT)) } {
            Ok(h) => h,
            Err(_) => return Ok(false),
        };
    // SAFETY: opened above.
    unsafe {
        let set = SetEvent(event);
        let _ = CloseHandle(event);
        set.context("signalling the agent to stop")?;
    }
    let deadline = std::time::Instant::now() + wait;
    while agent_running() {
        if std::time::Instant::now() >= deadline {
            bail!("the running agent did not stop within {} s", wait.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    info!("stopped the running agent");
    Ok(true)
}

/// The per-user autostart: an `HKCU\...\Run` value that starts `rd-agentw.exe` (next to this
/// executable) at login. The launcher runs `rd-agent run` without a console window and
/// restarts it when it fails, like launchd's KeepAlive on macOS.
pub mod service {
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use anyhow::{Context as _, bail};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    use super::{agent_running, log_dir, names, stop_agent};
    use crate::host::ServiceStatus;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "rd-agent";
    /// `DETACHED_PROCESS`: the launcher outlives this console.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    /// How long `install` / `uninstall` wait for a running agent to stop.
    const STOP_WAIT: Duration = Duration::from_secs(10);

    /// `rd-agentw.exe` next to this executable.
    fn launcher() -> anyhow::Result<PathBuf> {
        let exe = std::env::current_exe().context("locating the rd-agent executable")?;
        let launcher = exe.with_file_name(names::LAUNCHER_EXE);
        if !launcher.is_file() {
            bail!(
                "{} is missing next to {} (it is built with rd-agent: `cargo build --release -p \
                 rd-agent`)",
                names::LAUNCHER_EXE,
                exe.display()
            );
        }
        Ok(launcher)
    }

    fn run_value() -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf`/`size` describe a writable buffer.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    fn set_run_value(command: &str) -> anyhow::Result<()> {
        let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated UTF-16 string of the given byte length.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        rc.ok().context("writing the Run registry value")
    }

    /// The launcher path from a Run value: `"C:\...\rd-agentw.exe"`.
    fn launcher_in(command: &str) -> PathBuf {
        let path = match command.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or(rest),
            None => command.split(' ').next().unwrap_or(command),
        };
        PathBuf::from(path)
    }

    /// The command line for the Run value. `canonicalize` gives `\\?\C:\...`, which the shell
    /// runs fine but is ugly in Task Manager's startup list; plain drive paths are kept as such.
    fn command_for(launcher: &Path) -> String {
        let s = launcher.to_string_lossy();
        let plain = s
            .strip_prefix(r"\\?\")
            .filter(|p| p.as_bytes().get(1) == Some(&b':'));
        format!("\"{}\"", plain.unwrap_or(&s))
    }

    /// Registers the autostart and (re)starts the agent in the background.
    pub fn install() -> anyhow::Result<()> {
        let launcher = launcher()?;
        std::fs::create_dir_all(log_dir()?).context("creating the log directory")?;
        stop_agent(STOP_WAIT)?;
        let command = command_for(&launcher);
        set_run_value(&command)?;
        Command::new(&launcher)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", launcher.display()))?;
        println!("autostart installed: HKCU\\{RUN_KEY}\\{RUN_VALUE} = {command}");
        println!(
            "rd-agent now runs in the background (and at every login); logs: {}",
            log_dir()?.display()
        );
        Ok(())
    }

    /// Removes the autostart and stops the agent.
    pub fn uninstall() -> anyhow::Result<()> {
        // SAFETY: plain registry call.
        let rc = unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
            )
        };
        if rc == ERROR_SUCCESS {
            println!("autostart removed");
        } else if rc == ERROR_FILE_NOT_FOUND {
            println!("autostart was not installed");
        } else {
            rc.ok().context("removing the Run registry value")?;
        }
        if stop_agent(STOP_WAIT)? {
            println!("stopped the running agent");
        }
        if rd_platform_windows::secure::service::installed() {
            println!(
                "note: the secure desktop service is still installed; run \
                 `rd-agent secure uninstall` as administrator to remove it"
            );
        }
        Ok(())
    }

    pub fn status() -> anyhow::Result<ServiceStatus> {
        let value = run_value();
        let running = agent_running();
        Ok(ServiceStatus {
            installed: value.as_deref().map(launcher_in),
            loaded: value.is_some(),
            state: Some(if running { "running" } else { "not running" }.to_owned()),
            pid: None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn run_values_round_trip() {
            let cmd = command_for(Path::new(r"\\?\C:\Program Files\rd\rd-agentw.exe"));
            assert_eq!(cmd, r#""C:\Program Files\rd\rd-agentw.exe""#);
            assert_eq!(
                launcher_in(&cmd),
                PathBuf::from(r"C:\Program Files\rd\rd-agentw.exe")
            );
            // UNC paths keep their prefix.
            assert_eq!(
                command_for(Path::new(r"\\?\UNC\srv\share\rd-agentw.exe")),
                r#""\\?\UNC\srv\share\rd-agentw.exe""#
            );
            assert_eq!(
                launcher_in(r"C:\rd\rd-agentw.exe run"),
                PathBuf::from(r"C:\rd\rd-agentw.exe")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_agent_per_session() {
        // Another agent (a real install) may hold the mutex on a developer machine.
        if agent_running() {
            return;
        }
        let guard = RunGuard::acquire().unwrap();
        assert!(agent_running());
        let second = RunGuard::acquire()
            .err()
            .expect("a second guard is refused");
        assert!(second.is::<AlreadyRunning>());
        drop(guard);
        assert!(!agent_running());
    }

    #[test]
    fn locations_are_per_machine() {
        let config = config_path().unwrap();
        assert!(
            config.ends_with(r"rd-agent\config.toml"),
            "{}",
            config.display()
        );
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        assert!(config.starts_with(&local));
        assert!(log_dir().unwrap().starts_with(local));
    }

    #[test]
    fn rss_is_reported() {
        assert!(resident_bytes().is_some_and(|b| b > 0));
    }
}
