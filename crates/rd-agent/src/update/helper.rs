//! `rd-agent update-helper --plan <plan.json>`: installs an update, waits for the new version
//! to come back, and puts the previous one back if it does not (docs/updates.md, "rd agent").
//!
//! It runs from the copy of the old version in `previous/`, as its own process outside the
//! agent's login service (macOS: a temporary LaunchAgent; Windows: a detached process), so
//! that restarting or replacing the agent does not end it. The steps are platform independent
//! and touch the system only through [`Host`] and [`Sys`], so they are tested with fakes.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use rd_proto::control::{Perms, UpdateOutcome, UpdateResult};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use super::files::{Marker, UpdateDir, now_ms, read_json};

/// What the helper is to do, written by the agent that starts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub from: String,
    pub to: String,
    /// The update directory (markers, outcome, lock).
    pub dir: PathBuf,
    pub log_dir: PathBuf,
    /// Permissions the running version has; the new one must still have them (macOS).
    pub perms: Perms,
    /// Wait for this process to end before installing (Windows: `rd-agent update` run from a
    /// terminal keeps rd-agent.exe open, so the installer could not replace it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_pid: Option<u32>,
    pub limits: Limits,
    pub install: Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The new version must register with the gateway within this many seconds of the start.
    pub register_s: u64,
    /// ... and still run this many seconds after registering.
    pub settle_s: u64,
    /// After a rollback, how long to wait for the previous version (only to report it).
    pub rollback_s: u64,
    /// The Windows installer.
    pub installer_s: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            register_s: 120,
            settle_s: 10,
            rollback_s: 120,
            installer_s: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Install {
    /// Swap `RD Agent.app` and restart its LaunchAgent.
    Macos {
        /// The installed app, e.g. `/Applications/RD Agent.app`.
        app: PathBuf,
        /// The new version, unpacked and checked.
        new_app: PathBuf,
        /// A copy of the running app (for a rollback when the one moved aside is gone).
        previous_app: PathBuf,
        /// The agent's LaunchAgent and its domain (`gui/<uid>`).
        label: String,
        domain: String,
        /// The helper's own temporary LaunchAgent, removed when it is done.
        helper_label: String,
        helper_plist: PathBuf,
    },
    /// Run the installer, which stops the agent, replaces it and starts it again.
    Windows {
        /// The folder with rd-agent.exe, rd-agentw.exe and uninstall.exe.
        dir: PathBuf,
        /// The new version's setup.exe, checked.
        setup: PathBuf,
        /// Copies of the running rd-agent.exe and rd-agentw.exe.
        previous: PathBuf,
    },
}

/// The helper's view of time and of the agent.
pub trait Host {
    fn now_ms(&self) -> u64;
    fn sleep(&self, d: Duration);
    /// The newest `registered.json`.
    fn marker(&self) -> Option<Marker>;
    fn alive(&self, pid: u32) -> bool;
    /// Writes `outcome.json` (more than once when a later step adds to it).
    fn record(&self, outcome: &UpdateOutcome);
}

/// Puts a version in place and starts it.
pub trait Installer {
    /// Installs the new version and (re)starts the agent. An error means the new version was
    /// not started; whatever had changed is undone.
    fn install(&mut self) -> Result<(), String>;
    /// Puts the previous version back and starts it.
    fn roll_back(&mut self) -> Result<(), String>;
    /// After a successful update: removes what is no longer needed.
    fn clean_up(&mut self);
}

const POLL: Duration = Duration::from_secs(1);

/// Installs, waits for the new version and rolls back if needed. Records the outcome with
/// `host` and returns it.
pub fn run(plan: &Plan, host: &dyn Host, installer: &mut dyn Installer) -> UpdateOutcome {
    let (from, to) = (plan.from.as_str(), plan.to.as_str());
    let finish = |result: UpdateResult, error: String| {
        let outcome = UpdateOutcome {
            from: from.to_owned(),
            to: to.to_owned(),
            result,
            error,
            at_ms: host.now_ms(),
        };
        host.record(&outcome);
        outcome
    };
    info!(from, to, "installing the update");
    let started = host.now_ms();
    if let Err(e) = installer.install() {
        error!("installing {to} failed: {e}");
        return finish(UpdateResult::Failed, e);
    }
    let limits = plan.limits;
    let wait = Wait {
        limit: Duration::from_secs(limits.register_s),
        settle: Duration::from_secs(limits.settle_s),
    };
    let reason = match wait.registered(host, to, started, Some(plan.perms)) {
        Ok(()) => {
            info!("{to} is running and connected");
            installer.clean_up();
            return finish(UpdateResult::Ok, String::new());
        }
        Err(reason) => reason,
    };
    warn!("{reason}; putting {from} back");
    let rolled_back = host.now_ms();
    if let Err(e) = installer.roll_back() {
        error!("putting {from} back failed: {e}");
        return finish(
            UpdateResult::Failed,
            format!("{reason}; putting {from} back failed: {e}"),
        );
    }
    // Recorded now, so the previous version reports it as soon as it is connected.
    let mut outcome = finish(UpdateResult::RolledBack, reason);
    let wait = Wait {
        limit: Duration::from_secs(limits.rollback_s),
        ..wait
    };
    match wait.registered(host, from, rolled_back, None) {
        Ok(()) => info!("{from} is running again"),
        Err(e) => {
            error!("after the rollback: {e}");
            outcome.error = format!("{}; after the rollback {e}", outcome.error);
            outcome.at_ms = host.now_ms();
            host.record(&outcome);
        }
    }
    outcome
}

#[derive(Debug, Clone, Copy)]
struct Wait {
    limit: Duration,
    settle: Duration,
}

impl Wait {
    /// Waits until `version` registered after `since_ms` and kept running for the settle time.
    /// With `keep`, the permissions granted there must still be granted.
    fn registered(
        &self,
        host: &dyn Host,
        version: &str,
        since_ms: u64,
        keep: Option<Perms>,
    ) -> Result<(), String> {
        let deadline = since_ms + self.limit.as_millis() as u64;
        let settle = self.settle.as_millis() as u64;
        let mut seen = false;
        loop {
            let now = host.now_ms();
            let marker = host
                .marker()
                .filter(|m| m.version == version && m.at_ms >= since_ms);
            if let Some(m) = marker {
                seen = true;
                if let Some(lost) = keep.and_then(|keep| lost_permission(keep, m.perms)) {
                    return Err(format!(
                        "{version} does not have the {lost} permission any more (grant it again \
                         at the Mac)"
                    ));
                }
                if host.alive(m.pid) {
                    if now >= m.at_ms + settle {
                        return Ok(());
                    }
                    // Registered in time: it gets its settle time even past the deadline.
                    if m.at_ms <= deadline {
                        host.sleep(POLL);
                        continue;
                    }
                }
            }
            if now >= deadline {
                return Err(if seen {
                    format!("{version} connected but did not keep running")
                } else {
                    format!(
                        "{version} did not reconnect within {} s",
                        self.limit.as_secs()
                    )
                });
            }
            host.sleep(POLL);
        }
    }
}

fn lost_permission(before: Perms, now: Perms) -> Option<&'static str> {
    if before.screen && !now.screen {
        Some("Screen Recording")
    } else if before.input && !now.input {
        Some("Accessibility")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// installers

/// A program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Appended to the command line as is, without quoting (Windows: NSIS wants `/D=<dir>`
    /// last and unquoted, even with spaces).
    pub raw_tail: Option<OsString>,
}

impl Cmd {
    fn new(program: impl Into<PathBuf>, args: &[&std::ffi::OsStr]) -> Cmd {
        Cmd {
            program: program.into(),
            args: args.iter().map(|a| a.to_os_string()).collect(),
            raw_tail: None,
        }
    }
}

/// The system calls the installers make.
pub trait Sys {
    fn exists(&self, path: &Path) -> bool;
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()>;
    fn copy_file(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    /// Runs `cmd`, waiting at most `limit`; an error unless it exits with 0.
    fn run(&self, cmd: &Cmd, limit: Duration) -> anyhow::Result<()>;
    fn sleep(&self, d: Duration);
    /// Windows: ends every process whose program lives in `dir`.
    fn stop_programs_in(&self, dir: &Path) -> anyhow::Result<()>;
    /// Windows: the version shown in Settings > Apps.
    fn set_display_version(&self, version: &str) -> anyhow::Result<()>;
}

const LAUNCHCTL: &str = "/bin/launchctl";
const DITTO: &str = "/usr/bin/ditto";
/// `ditto` and `launchctl` finish in seconds.
const MAC_COMMAND_LIMIT: Duration = Duration::from_secs(120);

/// macOS: swaps `RD Agent.app` with two renames in its folder (the old one stays next to it
/// until the new one has proven itself), then restarts the LaunchAgent.
pub struct MacSwap<'a> {
    pub sys: &'a dyn Sys,
    pub app: PathBuf,
    pub new_app: PathBuf,
    pub previous_app: PathBuf,
    pub label: String,
    pub domain: String,
    pub from: String,
    pub to: String,
}

impl MacSwap<'_> {
    /// `RD Agent.app.<suffix>` next to the app: not an `.app` any more, so LaunchServices
    /// leaves it alone.
    fn sibling(&self, suffix: &str) -> PathBuf {
        let name = self.app.file_name().unwrap_or_default().to_string_lossy();
        self.app.with_file_name(format!("{name}.{suffix}"))
    }

    fn aside(&self) -> PathBuf {
        self.sibling(&format!("previous-{}", self.from))
    }

    fn staged(&self) -> PathBuf {
        self.sibling(&format!("new-{}", self.to))
    }

    fn failed(&self) -> PathBuf {
        self.sibling(&format!("failed-{}", self.to))
    }

    fn remove(&self, path: &Path) {
        if self.sys.exists(path)
            && let Err(e) = self.sys.remove_dir_all(path)
        {
            warn!("removing {}: {e}", path.display());
        }
    }

    fn ditto(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        let cmd = Cmd::new(DITTO, &[from.as_os_str(), to.as_os_str()]);
        self.sys.run(&cmd, MAC_COMMAND_LIMIT)
    }

    fn restart(&self) -> anyhow::Result<()> {
        let target = format!("{}/{}", self.domain, self.label);
        let cmd = Cmd::new(
            LAUNCHCTL,
            &["kickstart".as_ref(), "-k".as_ref(), target.as_ref()],
        );
        self.sys.run(&cmd, MAC_COMMAND_LIMIT)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        self.sys.rename(from, to).map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (allow RD Agent under System Settings > Privacy & Security > App Management)"
            } else {
                ""
            };
            format!("moving {} to {}: {e}{hint}", from.display(), to.display())
        })
    }
}

impl Installer for MacSwap<'_> {
    fn install(&mut self) -> Result<(), String> {
        let (aside, staged) = (self.aside(), self.staged());
        // Leftovers of an earlier attempt that was cut short.
        for path in [&staged, &self.failed(), &aside] {
            self.remove(path);
        }
        // Copied next to the app first, so the swap is two renames on one volume.
        if let Err(e) = self.ditto(&self.new_app, &staged) {
            self.remove(&staged);
            return Err(format!("copying the new version next to the app: {e:#}"));
        }
        if let Err(e) = self.rename(&self.app, &aside) {
            self.remove(&staged);
            return Err(e);
        }
        if let Err(e) = self.rename(&staged, &self.app) {
            if let Err(back) = self.rename(&aside, &self.app) {
                error!("{back}");
            }
            self.remove(&staged);
            return Err(e);
        }
        info!("{} replaced; restarting the agent", self.app.display());
        if let Err(e) = self.restart() {
            // Not started: the old app goes back (it is still the one running, if any).
            let failed = self.failed();
            if self.rename(&self.app, &failed).is_ok() && self.rename(&aside, &self.app).is_ok() {
                self.remove(&failed);
            }
            let _ = self.restart();
            return Err(format!("restarting the agent: {e:#}"));
        }
        Ok(())
    }

    fn roll_back(&mut self) -> Result<(), String> {
        let (aside, failed) = (self.aside(), self.failed());
        self.remove(&failed);
        if self.sys.exists(&self.app) {
            self.rename(&self.app, &failed)?;
        }
        if self.sys.exists(&aside) {
            self.rename(&aside, &self.app)?;
        } else {
            warn!(
                "{} is gone; restoring {}",
                aside.display(),
                self.previous_app.display()
            );
            self.ditto(&self.previous_app, &self.app)
                .map_err(|e| format!("restoring {}: {e:#}", self.app.display()))?;
        }
        self.restart()
            .map_err(|e| format!("restarting the agent: {e:#}"))?;
        self.remove(&failed);
        Ok(())
    }

    fn clean_up(&mut self) {
        self.remove(&self.aside());
    }
}

/// Windows: runs the installer (which stops the agent with `rd-agent uninstall`, replaces the
/// programs and starts it again with `rd-agent install`); a rollback copies the previous
/// programs back the same way.
pub struct WinSetup<'a> {
    pub sys: &'a dyn Sys,
    pub dir: PathBuf,
    pub setup: PathBuf,
    pub previous: PathBuf,
    pub from: String,
    pub installer_limit: Duration,
}

/// The programs a rollback puts back (the rest of the folder does not depend on the version).
const WIN_PROGRAMS: [&str; 2] = ["rd-agent.exe", "rd-agentw.exe"];
/// `rd-agent install` / `uninstall` wait up to 10 s for a running agent to stop.
const WIN_COMMAND_LIMIT: Duration = Duration::from_secs(60);
const COPY_TRIES: u32 = 10;

impl WinSetup<'_> {
    fn agent(&self, dir: &Path, command: &str) -> anyhow::Result<()> {
        let cmd = Cmd::new(dir.join("rd-agent.exe"), &[command.as_ref()]);
        self.sys.run(&cmd, WIN_COMMAND_LIMIT)
    }

    /// Puts the previous programs back and starts them.
    fn restore(&mut self) -> Result<(), String> {
        // Stop whatever runs from the folder and remove its autostart. The previous copy's
        // `uninstall` signals the same stop event, should the new rd-agent.exe not run at all.
        if let Err(e) = self.agent(&self.dir, "uninstall") {
            warn!("the new rd-agent.exe uninstall: {e:#}");
            if let Err(e) = self.agent(&self.previous, "uninstall") {
                warn!("the previous rd-agent.exe uninstall: {e:#}");
            }
        }
        // A launcher restarting a crashing agent, or an agent that did not stop.
        if let Err(e) = self.sys.stop_programs_in(&self.dir) {
            warn!("stopping the programs in {}: {e:#}", self.dir.display());
        }
        for name in WIN_PROGRAMS {
            let (from, to) = (self.previous.join(name), self.dir.join(name));
            let mut tries = 0;
            // Windows keeps a program's file locked for a moment after it exited.
            while let Err(e) = self.sys.copy_file(&from, &to) {
                tries += 1;
                if tries >= COPY_TRIES {
                    return Err(format!(
                        "copying {} to {}: {e}",
                        from.display(),
                        to.display()
                    ));
                }
                self.sys.sleep(Duration::from_secs(1));
            }
        }
        self.agent(&self.dir, "install")
            .map_err(|e| format!("starting the previous version: {e:#}"))?;
        if let Err(e) = self.sys.set_display_version(&self.from) {
            warn!("resetting the version in Settings > Apps: {e:#}");
        }
        Ok(())
    }
}

impl Installer for WinSetup<'_> {
    fn install(&mut self) -> Result<(), String> {
        let mut cmd = Cmd::new(&self.setup, &["/S".as_ref()]);
        let mut dir = OsString::from("/D=");
        dir.push(&self.dir);
        cmd.raw_tail = Some(dir);
        let Err(e) = self.sys.run(&cmd, self.installer_limit) else {
            return Ok(());
        };
        // The installer may have stopped the agent or replaced files before it failed.
        let restored = match self.restore() {
            Ok(()) => String::new(),
            Err(r) => format!("; putting the previous version back failed: {r}"),
        };
        Err(format!("the installer failed: {e:#}{restored}"))
    }

    fn roll_back(&mut self) -> Result<(), String> {
        self.restore()
    }

    fn clean_up(&mut self) {}
}

// ---------------------------------------------------------------------------
// the process

/// `rd-agent update-helper --plan <path>`.
pub fn main(plan_path: &Path) -> anyhow::Result<()> {
    let plan: Plan = read_json(plan_path)
        .with_context(|| format!("reading the update plan {}", plan_path.display()))?;
    let log = open_log(&plan.log_dir);
    init_logging(log.as_ref());
    info!(
        from = plan.from,
        to = plan.to,
        pid = std::process::id(),
        "update helper started"
    );
    let dir = UpdateDir::new(plan.dir.clone());
    if let Err(e) = dir.set_helper_pid(std::process::id()) {
        warn!("recording the helper in the update lock: {e:#}");
    }
    if let Some(pid) = plan.wait_pid {
        wait_for_exit(pid);
    }
    let sys = RealSys {
        log: log.as_ref().and_then(|f| f.try_clone().ok()),
    };
    let host = RealHost { dir: &dir };
    let outcome = match &plan.install {
        Install::Macos {
            app,
            new_app,
            previous_app,
            label,
            domain,
            ..
        } => run(
            &plan,
            &host,
            &mut MacSwap {
                sys: &sys,
                app: app.clone(),
                new_app: new_app.clone(),
                previous_app: previous_app.clone(),
                label: label.clone(),
                domain: domain.clone(),
                from: plan.from.clone(),
                to: plan.to.clone(),
            },
        ),
        Install::Windows {
            dir: install_dir,
            setup,
            previous,
        } => run(
            &plan,
            &host,
            &mut WinSetup {
                sys: &sys,
                dir: install_dir.clone(),
                setup: setup.clone(),
                previous: previous.clone(),
                from: plan.from.clone(),
                installer_limit: Duration::from_secs(plan.limits.installer_s),
            },
        ),
    };
    info!(result = ?outcome.result, error = outcome.error, "update finished");
    // The downloaded files are of no use any more; the copy in previous/ is replaced by the
    // next update (on Windows this process still runs from it).
    let _ = std::fs::remove_dir_all(dir.download());
    let _ = std::fs::remove_file(dir.plan());
    dir.release_lock();
    if let Install::Macos {
        domain,
        helper_label,
        helper_plist,
        ..
    } = &plan.install
    {
        // Last: launchd ends this process when it removes the job.
        let _ = std::fs::remove_file(helper_plist);
        info!("removing the helper's LaunchAgent");
        let target = format!("{domain}/{helper_label}");
        let _ = sys.run(
            &Cmd::new(LAUNCHCTL, &["bootout".as_ref(), target.as_ref()]),
            MAC_COMMAND_LIMIT,
        );
    }
    Ok(())
}

fn open_log(dir: &Path) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("update-helper.log"))
        .ok()
}

fn init_logging(log: Option<&File>) {
    let Some(file) = log.and_then(|f| f.try_clone().ok()) else {
        crate::logging::init_cli_verbose();
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
}

fn wait_for_exit(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while process_alive(pid) {
        if std::time::Instant::now() >= deadline {
            warn!(pid, "process {pid} still runs; installing anyway");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct RealHost<'a> {
    dir: &'a UpdateDir,
}

impl Host for RealHost<'_> {
    fn now_ms(&self) -> u64 {
        now_ms()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn marker(&self) -> Option<Marker> {
        self.dir.read_marker()
    }

    fn alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    fn record(&self, outcome: &UpdateOutcome) {
        if let Err(e) = self.dir.write_outcome(outcome) {
            error!("writing the outcome: {e:#}");
        }
    }
}

struct RealSys {
    /// The helper's log; commands write their output there too.
    log: Option<File>,
}

impl Sys for RealSys {
    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn copy_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::copy(from, to).map(|_| ())
    }

    fn run(&self, cmd: &Cmd, limit: Duration) -> anyhow::Result<()> {
        use std::process::{Command, Stdio};

        info!(program = %cmd.program.display(), args = ?cmd.args, tail = ?cmd.raw_tail, "running");
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args).stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            if let Some(tail) = &cmd.raw_tail {
                command.raw_arg(tail);
            }
            command.creation_flags(sys::CREATE_NO_WINDOW);
        }
        #[cfg(not(windows))]
        if let Some(tail) = &cmd.raw_tail {
            command.arg(tail);
        }
        let output = |f: &Option<File>| {
            f.as_ref()
                .and_then(|f| f.try_clone().ok())
                .map_or_else(Stdio::null, Stdio::from)
        };
        command.stdout(output(&self.log)).stderr(output(&self.log));
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", cmd.program.display()))?;
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(
                    status.success(),
                    "{} exited with {status}",
                    cmd.program.display()
                );
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "{} did not finish within {} s",
                    cmd.program.display(),
                    limit.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn stop_programs_in(&self, dir: &Path) -> anyhow::Result<()> {
        sys::stop_programs_in(dir)
    }

    fn set_display_version(&self, version: &str) -> anyhow::Result<()> {
        sys::set_display_version(version)
    }
}

pub fn process_alive(pid: u32) -> bool {
    sys::process_alive(pid)
}

#[cfg(target_os = "macos")]
mod sys {
    use std::path::Path;

    pub fn process_alive(pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub fn stop_programs_in(_dir: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn set_display_version(_version: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::{Path, PathBuf};

    use anyhow::Context as _;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    };
    use windows::core::{HSTRING, PWSTR};

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// Where the installer registers the agent for Settings > Apps.
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\rd-agent";

    pub fn process_alive(pid: u32) -> bool {
        // SAFETY: the handle is closed below.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut code = 0u32;
        // SAFETY: `handle` is a process handle opened above.
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        // SAFETY: opened above, closed once.
        let _ = unsafe { CloseHandle(handle) };
        ok && code == STILL_ACTIVE.0 as u32
    }

    fn image_path(process: HANDLE) -> Option<PathBuf> {
        let mut buf = vec![0u16; 32 * 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `buf`/`len` describe a writable buffer.
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
    }

    fn same_dir(a: &Path, b: &Path) -> bool {
        let norm = |p: &Path| {
            p.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        norm(a) == norm(b)
    }

    /// Ends rd-agent.exe and rd-agentw.exe processes started from `dir`.
    pub fn stop_programs_in(dir: &Path) -> anyhow::Result<()> {
        // SAFETY: a snapshot handle, closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .context("listing processes")?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let own = std::process::id();
        // SAFETY: `entry` has its size set; the snapshot is valid.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            let pid = entry.th32ProcessID;
            if pid != own && (name == "rd-agent.exe" || name == "rd-agentw.exe") {
                // SAFETY: the handle is closed below.
                if let Ok(process) = unsafe {
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                        false,
                        pid,
                    )
                } {
                    if let Some(path) = image_path(process)
                        && path.parent().is_some_and(|p| same_dir(p, dir))
                    {
                        tracing::info!(pid, path = %path.display(), "ending");
                        // SAFETY: a process handle with PROCESS_TERMINATE.
                        let _ = unsafe { TerminateProcess(process, 1) };
                    }
                    // SAFETY: opened above.
                    let _ = unsafe { CloseHandle(process) };
                }
            }
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        // SAFETY: created above.
        let _ = unsafe { CloseHandle(snapshot) };
        Ok(())
    }

    pub fn set_display_version(version: &str) -> anyhow::Result<()> {
        let data: Vec<u16> = version.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated UTF-16 string of the given byte length.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(UNINSTALL_KEY),
                &HSTRING::from("DisplayVersion"),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        rc.ok().context("writing DisplayVersion")
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod sys {
    use std::path::Path;

    pub fn process_alive(_pid: u32) -> bool {
        false
    }

    pub fn stop_programs_in(_dir: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn set_display_version(_version: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeSet, VecDeque};

    use super::*;

    const FROM: &str = "1.5.0";
    const TO: &str = "1.6.0";
    const START: u64 = 1_000_000;
    const ALL: Perms = Perms {
        screen: true,
        input: true,
    };

    fn plan(install: Install) -> Plan {
        Plan {
            from: FROM.into(),
            to: TO.into(),
            dir: "/data/update".into(),
            log_dir: "/logs".into(),
            perms: ALL,
            wait_pid: None,
            limits: Limits::default(),
            install,
        }
    }

    fn mac_plan() -> Plan {
        plan(Install::Macos {
            app: "/Apps/RD Agent.app".into(),
            new_app: "/data/update/download/unpacked/RD Agent.app".into(),
            previous_app: "/data/update/previous/RD Agent.app".into(),
            label: "dev.thijmen.rd.agent".into(),
            domain: "gui/501".into(),
            helper_label: "dev.thijmen.rd.agent.update".into(),
            helper_plist: "/data/update/dev.thijmen.rd.agent.update.plist".into(),
        })
    }

    /// Time passes only when the helper sleeps; markers appear on a schedule.
    struct FakeHost {
        now: Cell<u64>,
        /// (from this time on, marker)
        markers: RefCell<Vec<(u64, Marker)>>,
        dead: RefCell<BTreeSet<u32>>,
        recorded: RefCell<Vec<UpdateOutcome>>,
    }

    impl FakeHost {
        fn new() -> FakeHost {
            FakeHost {
                now: Cell::new(START),
                markers: RefCell::default(),
                dead: RefCell::default(),
                recorded: RefCell::default(),
            }
        }

        /// `version` registers `after_s` seconds from the start.
        fn registers(&self, after_s: u64, version: &str, pid: u32, perms: Perms) {
            let at = START + after_s * 1000;
            self.markers.borrow_mut().push((
                at,
                Marker {
                    version: version.into(),
                    pid,
                    at_ms: at,
                    perms,
                    service: None,
                },
            ));
        }
    }

    impl Host for FakeHost {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }

        fn sleep(&self, d: Duration) {
            self.now.set(self.now.get() + d.as_millis() as u64);
        }

        fn marker(&self) -> Option<Marker> {
            let now = self.now.get();
            let markers = self.markers.borrow();
            markers
                .iter()
                .filter(|(from, _)| *from <= now)
                .max_by_key(|(from, _)| *from)
                .map(|(_, m)| m.clone())
        }

        fn alive(&self, pid: u32) -> bool {
            !self.dead.borrow().contains(&pid)
        }

        fn record(&self, outcome: &UpdateOutcome) {
            self.recorded.borrow_mut().push(outcome.clone());
        }
    }

    /// Steps taken, with scripted failures.
    #[derive(Default)]
    struct FakeInstaller {
        steps: Vec<&'static str>,
        install: Option<String>,
        roll_back: Option<String>,
    }

    impl Installer for FakeInstaller {
        fn install(&mut self) -> Result<(), String> {
            self.steps.push("install");
            self.install.clone().map_or(Ok(()), Err)
        }

        fn roll_back(&mut self) -> Result<(), String> {
            self.steps.push("roll_back");
            self.roll_back.clone().map_or(Ok(()), Err)
        }

        fn clean_up(&mut self) {
            self.steps.push("clean_up");
        }
    }

    fn update(host: &FakeHost, installer: &mut FakeInstaller) -> UpdateOutcome {
        let outcome = run(&mac_plan(), host, installer);
        assert_eq!(host.recorded.borrow().last(), Some(&outcome));
        assert_eq!((outcome.from.as_str(), outcome.to.as_str()), (FROM, TO));
        outcome
    }

    #[test]
    fn the_new_version_comes_back() {
        let host = FakeHost::new();
        // The old version's marker from before the update does not count.
        host.registers(0, FROM, 10, ALL);
        host.registers(8, TO, 11, ALL);
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::Ok, "{}", outcome.error);
        assert_eq!(outcome.error, "");
        assert_eq!(installer.steps, ["install", "clean_up"]);
        // Registered at 8 s, plus 10 s of running.
        assert_eq!(outcome.at_ms, START + 18_000);
    }

    #[test]
    fn rolls_back_when_the_new_version_does_not_connect() {
        let host = FakeHost::new();
        host.registers(0, FROM, 10, ALL);
        // The previous version is back 5 s after the rollback started.
        host.registers(125, FROM, 12, ALL);
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::RolledBack);
        assert_eq!(outcome.error, "1.6.0 did not reconnect within 120 s");
        assert_eq!(installer.steps, ["install", "roll_back"]);
        // Recorded once, right after the rollback (so the old version can report it).
        assert_eq!(host.recorded.borrow().len(), 1);
        assert_eq!(outcome.at_ms, START + 120_000);
    }

    #[test]
    fn rolls_back_a_version_that_keeps_crashing() {
        let host = FakeHost::new();
        // Registers, then dies; launchd restarts it, it registers and dies again, ...
        for (i, at) in (5..120).step_by(15).enumerate() {
            let pid = 100 + i as u32;
            host.registers(at, TO, pid, ALL);
            host.dead.borrow_mut().insert(pid);
        }
        host.registers(130, FROM, 7, ALL);
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::RolledBack);
        assert_eq!(outcome.error, "1.6.0 connected but did not keep running");
        assert_eq!(installer.steps, ["install", "roll_back"]);
    }

    #[test]
    fn a_late_registration_gets_its_settle_time() {
        let host = FakeHost::new();
        host.registers(119, TO, 11, ALL);
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::Ok, "{}", outcome.error);
        assert_eq!(outcome.at_ms, START + 129_000);
    }

    #[test]
    fn rolls_back_when_a_permission_is_lost() {
        let host = FakeHost::new();
        host.registers(
            6,
            TO,
            11,
            Perms {
                screen: true,
                input: false,
            },
        );
        host.registers(20, FROM, 12, ALL);
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::RolledBack);
        assert!(
            outcome.error.contains("Accessibility permission"),
            "{}",
            outcome.error
        );
        // Rolled back right away, not after the full wait.
        assert_eq!(outcome.at_ms, START + 6_000);

        // A permission that was not granted before cannot be lost.
        let host = FakeHost::new();
        host.registers(
            6,
            TO,
            11,
            Perms {
                screen: true,
                input: false,
            },
        );
        let mut p = mac_plan();
        p.perms.input = false;
        let outcome = run(&p, &host, &mut FakeInstaller::default());
        assert_eq!(outcome.result, UpdateResult::Ok, "{}", outcome.error);
    }

    #[test]
    fn a_failed_install_is_reported_as_failed() {
        let host = FakeHost::new();
        let mut installer = FakeInstaller {
            install: Some("the installer failed: exit code 2".into()),
            ..Default::default()
        };
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::Failed);
        assert_eq!(outcome.error, "the installer failed: exit code 2");
        assert_eq!(installer.steps, ["install"]);
    }

    #[test]
    fn a_failed_rollback_is_reported() {
        let host = FakeHost::new();
        let mut installer = FakeInstaller {
            roll_back: Some("restarting the agent: launchctl failed".into()),
            ..Default::default()
        };
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::Failed);
        assert_eq!(
            outcome.error,
            "1.6.0 did not reconnect within 120 s; putting 1.5.0 back failed: restarting the \
             agent: launchctl failed"
        );
    }

    #[test]
    fn the_outcome_says_when_the_previous_version_did_not_return_either() {
        let host = FakeHost::new();
        let mut installer = FakeInstaller::default();
        let outcome = update(&host, &mut installer);
        assert_eq!(outcome.result, UpdateResult::RolledBack);
        assert_eq!(
            outcome.error,
            "1.6.0 did not reconnect within 120 s; after the rollback 1.5.0 did not reconnect \
             within 120 s"
        );
        assert_eq!(host.recorded.borrow().len(), 2);
    }

    // -- installers over a fake system

    /// A file system of paths (renames move whole trees) and a log of what was done.
    #[derive(Default)]
    struct FakeSys {
        paths: RefCell<BTreeSet<PathBuf>>,
        log: RefCell<Vec<String>>,
        /// Steps (by the start of their log line) that fail, each once.
        fail: RefCell<VecDeque<String>>,
    }

    impl FakeSys {
        fn with(paths: &[&str]) -> FakeSys {
            let sys = FakeSys::default();
            sys.paths
                .borrow_mut()
                .extend(paths.iter().map(PathBuf::from));
            sys
        }

        fn fail(&self, step: &str) {
            self.fail.borrow_mut().push_back(step.to_owned());
        }

        fn step(&self, line: String) -> std::io::Result<()> {
            self.log.borrow_mut().push(line.clone());
            let mut fail = self.fail.borrow_mut();
            if let Some(i) = fail.iter().position(|f| line.starts_with(f.as_str())) {
                fail.remove(i);
                return Err(std::io::Error::other("scripted failure"));
            }
            Ok(())
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn has(&self, path: &str) -> bool {
            self.paths.borrow().contains(Path::new(path))
        }
    }

    impl Sys for FakeSys {
        fn exists(&self, path: &Path) -> bool {
            self.paths.borrow().contains(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.step(format!("rename {} -> {}", from.display(), to.display()))?;
            let mut paths = self.paths.borrow_mut();
            if !paths.contains(from) {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            let moved: Vec<PathBuf> = paths
                .iter()
                .filter(|p| p.starts_with(from))
                .cloned()
                .collect();
            for p in moved {
                paths.remove(&p);
                paths.insert(to.join(p.strip_prefix(from).unwrap()));
            }
            // `join("")` adds no separator, so `to` itself is in the set.
            Ok(())
        }

        fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.step(format!("remove {}", path.display()))?;
            self.paths.borrow_mut().retain(|p| !p.starts_with(path));
            Ok(())
        }

        fn copy_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.step(format!("copy {} -> {}", from.display(), to.display()))?;
            self.paths.borrow_mut().insert(to.to_owned());
            Ok(())
        }

        fn run(&self, cmd: &Cmd, _limit: Duration) -> anyhow::Result<()> {
            let mut line = format!("run {}", cmd.program.display());
            for arg in &cmd.args {
                line.push(' ');
                line.push_str(&arg.to_string_lossy());
            }
            if let Some(tail) = &cmd.raw_tail {
                line.push_str(&format!(" [raw {}]", tail.to_string_lossy()));
            }
            self.step(line)?;
            if cmd.program == Path::new(DITTO) {
                self.paths.borrow_mut().insert(PathBuf::from(&cmd.args[1]));
            }
            Ok(())
        }

        fn sleep(&self, _d: Duration) {}

        fn stop_programs_in(&self, dir: &Path) -> anyhow::Result<()> {
            Ok(self.step(format!("stop {}", dir.display()))?)
        }

        fn set_display_version(&self, version: &str) -> anyhow::Result<()> {
            Ok(self.step(format!("display-version {version}"))?)
        }
    }

    const APP: &str = "/Apps/RD Agent.app";
    const NEW: &str = "/data/update/download/unpacked/RD Agent.app";
    const PREV: &str = "/data/update/previous/RD Agent.app";

    fn mac(sys: &FakeSys) -> MacSwap<'_> {
        MacSwap {
            sys,
            app: APP.into(),
            new_app: NEW.into(),
            previous_app: PREV.into(),
            label: "dev.thijmen.rd.agent".into(),
            domain: "gui/501".into(),
            from: FROM.into(),
            to: TO.into(),
        }
    }

    #[test]
    fn mac_swaps_the_app_and_restarts_the_launch_agent() {
        let sys = FakeSys::with(&[APP, NEW, PREV]);
        let mut swap = mac(&sys);
        swap.install().unwrap();
        assert_eq!(
            sys.log(),
            [
                format!("run {DITTO} {NEW} {APP}.new-1.6.0"),
                format!("rename {APP} -> {APP}.previous-1.5.0"),
                format!("rename {APP}.new-1.6.0 -> {APP}"),
                format!("run {LAUNCHCTL} kickstart -k gui/501/dev.thijmen.rd.agent"),
            ]
        );
        swap.clean_up();
        assert!(sys.has(APP));
        assert!(!sys.has(&format!("{APP}.previous-1.5.0")));
    }

    #[test]
    fn mac_rolls_back_to_the_app_moved_aside() {
        let sys = FakeSys::with(&[APP, NEW, PREV]);
        let mut swap = mac(&sys);
        swap.install().unwrap();
        sys.log.borrow_mut().clear();
        swap.roll_back().unwrap();
        assert_eq!(
            sys.log(),
            [
                format!("rename {APP} -> {APP}.failed-1.6.0"),
                format!("rename {APP}.previous-1.5.0 -> {APP}"),
                format!("run {LAUNCHCTL} kickstart -k gui/501/dev.thijmen.rd.agent"),
                format!("remove {APP}.failed-1.6.0"),
            ]
        );
        assert!(sys.has(APP));
        assert!(!sys.has(&format!("{APP}.failed-1.6.0")));

        // Without the app moved aside: the copy in previous/.
        let sys = FakeSys::with(&[APP, PREV]);
        mac(&sys).roll_back().unwrap();
        assert!(
            sys.log().contains(&format!("run {DITTO} {PREV} {APP}")),
            "{:?}",
            sys.log()
        );
    }

    #[test]
    fn mac_undoes_a_swap_that_cannot_finish() {
        // The app cannot be moved (App Management): nothing changes.
        let sys = FakeSys::with(&[APP, NEW, PREV]);
        sys.fail(&format!("rename {APP} ->"));
        let err = mac(&sys).install().unwrap_err();
        assert!(err.starts_with("moving /Apps/RD Agent.app"), "{err}");
        assert!(sys.has(APP));
        assert!(!sys.has(&format!("{APP}.new-1.6.0")));

        // The new app cannot be moved in: the old one goes back.
        let sys = FakeSys::with(&[APP, NEW, PREV]);
        sys.fail(&format!("rename {APP}.new-1.6.0 -> {APP}"));
        mac(&sys).install().unwrap_err();
        assert!(sys.has(APP));
        assert!(!sys.has(&format!("{APP}.previous-1.5.0")));

        // launchctl fails: the old app goes back and is started again.
        let sys = FakeSys::with(&[APP, NEW, PREV]);
        sys.fail(&format!("run {LAUNCHCTL}"));
        let err = mac(&sys).install().unwrap_err();
        assert!(err.starts_with("restarting the agent"), "{err}");
        assert!(sys.has(APP));
        assert!(!sys.has(&format!("{APP}.previous-1.5.0")));
        assert!(!sys.has(&format!("{APP}.failed-1.6.0")));
        assert_eq!(
            sys.log().last().unwrap(),
            &format!("run {LAUNCHCTL} kickstart -k gui/501/dev.thijmen.rd.agent")
        );
    }

    const DIR: &str = r"C:\Users\me\AppData\Local\Programs\rd-agent";

    fn win(sys: &FakeSys) -> WinSetup<'_> {
        WinSetup {
            sys,
            dir: DIR.into(),
            setup: "/data/update/download/rd-agent-1.6.0-windows-x64-setup.exe".into(),
            previous: "/data/update/previous".into(),
            from: FROM.into(),
            installer_limit: Duration::from_secs(300),
        }
    }

    fn win_path(dir: &str, name: &str) -> String {
        Path::new(dir).join(name).display().to_string()
    }

    #[test]
    fn windows_runs_the_installer_silently_into_the_same_folder() {
        let sys = FakeSys::default();
        win(&sys).install().unwrap();
        assert_eq!(
            sys.log(),
            [format!(
                "run /data/update/download/rd-agent-1.6.0-windows-x64-setup.exe /S [raw /D={DIR}]"
            )]
        );
    }

    #[test]
    fn windows_rolls_back_by_restoring_the_programs() {
        let sys = FakeSys::default();
        win(&sys).roll_back().unwrap();
        let prev = "/data/update/previous";
        assert_eq!(
            sys.log(),
            [
                format!("run {} uninstall", win_path(DIR, "rd-agent.exe")),
                format!("stop {DIR}"),
                format!(
                    "copy {} -> {}",
                    win_path(prev, "rd-agent.exe"),
                    win_path(DIR, "rd-agent.exe")
                ),
                format!(
                    "copy {} -> {}",
                    win_path(prev, "rd-agentw.exe"),
                    win_path(DIR, "rd-agentw.exe")
                ),
                format!("run {} install", win_path(DIR, "rd-agent.exe")),
                "display-version 1.5.0".to_owned(),
            ]
        );

        // A broken new rd-agent.exe: the previous one stops the agent; locked files are
        // retried.
        let sys = FakeSys::default();
        sys.fail(&format!("run {} uninstall", win_path(DIR, "rd-agent.exe")));
        sys.fail("copy");
        sys.fail("copy");
        win(&sys).roll_back().unwrap();
        let log = sys.log();
        assert_eq!(
            log[1],
            format!("run {} uninstall", win_path(prev, "rd-agent.exe"))
        );
        assert_eq!(log.iter().filter(|l| l.starts_with("copy")).count(), 4);
    }

    #[test]
    fn windows_puts_the_previous_version_back_when_the_installer_fails() {
        let sys = FakeSys::default();
        sys.fail("run /data/update/download/");
        let err = win(&sys).install().unwrap_err();
        assert!(err.starts_with("the installer failed"), "{err}");
        assert!(
            sys.log()
                .contains(&format!("run {} install", win_path(DIR, "rd-agent.exe")))
        );

        // ... and says so when that fails too.
        let sys = FakeSys::default();
        sys.fail("run /data/update/download/");
        sys.fail(&format!("run {} install", win_path(DIR, "rd-agent.exe")));
        let err = win(&sys).install().unwrap_err();
        assert!(
            err.contains("putting the previous version back failed: starting the previous version"),
            "{err}"
        );
    }

    #[test]
    fn plans_round_trip() {
        let p = mac_plan();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""kind":"macos""#), "{json}");
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), p);
    }
}
