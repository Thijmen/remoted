//! The `rd-agent run` side of updates: checks the manifest (1 minute after start, then every
//! 6 hours, unless `[update] check = false`), keeps `update_status` current, writes the
//! registered marker, and installs when the gateway sends `update`.

use std::sync::Arc;
use std::time::Duration;

use rd_agent_core::runtime::{SessionControl, UpdateEvent, Updates};
use rd_proto::control::{Perms, UpdateOutcome, UpdatePhase, UpdateStatus};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{error, info, warn};

use super::files::{Marker, now_ms};
use super::manifest::Manifest;
use super::prepare::Updater;
use super::target::{self, Mode, Target};

const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How often outcome.json and helper.lock are looked at (the helper writes them).
const POLL: Duration = Duration::from_secs(5);
/// How long the current session gets to end before the helper starts.
const SESSION_END_WAIT: Duration = Duration::from_secs(5);
/// The helper records itself in the update lock within this time of being started.
const HELPER_START: Duration = Duration::from_secs(60);

/// What the background work came back with.
enum Done {
    Check(anyhow::Result<Manifest>),
    /// The update ended without replacing this process: its outcome, or why there is none.
    Install(Result<UpdateOutcome, String>),
}

struct Service {
    updater: Arc<Updater>,
    status: watch::Sender<UpdateStatus>,
    sessions: SessionControl,
    /// The installed agent this one is, or why it cannot update itself.
    target: Result<Target, String>,
    /// Permissions at the last registration; a new version must keep them.
    perms: Perms,
    job: Option<JoinHandle<Done>>,
    /// The job is a check. Checks take a second and are not reported as a phase: every 6 hours
    /// two phase changes per agent would only fill the gateway's log.
    checking: bool,
    /// An `update` that came while a check ran.
    queued: Option<String>,
}

/// Runs until the runtime drops its end of `updates`.
pub async fn run(updates: Updates, updater: Arc<Updater>, check: bool) {
    let Updates {
        status,
        mut events,
        sessions,
    } = updates;
    let detect_dir = updater.dir.clone();
    let target = tokio::task::spawn_blocking(move || target::detect(Mode::Service, &detect_dir))
        .await
        .unwrap_or_else(|e| Err(format!("checking the installation: {e}")));
    match &target {
        Ok(t) => info!(?t, "this agent can install updates"),
        Err(reason) => info!(reason, "this agent cannot install updates"),
    }
    let mut service = Service {
        updater,
        status,
        sessions,
        target,
        perms: Perms::default(),
        job: None,
        checking: false,
        queued: None,
    };
    service.status.send_modify(|s| {
        s.can_install = service.target.is_ok();
        s.cannot_install = service.target.clone().err().unwrap_or_default();
    });
    service.poll_files();

    let mut next_check = Instant::now() + FIRST_CHECK;
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = sleep_until(next_check), if check => {
                next_check = Instant::now() + CHECK_INTERVAL;
                if service.job.is_none() {
                    service.start_check();
                }
            }
            event = events.recv() => match event {
                None => return,
                Some(UpdateEvent::Registered { state }) => service.registered(state.perms),
                Some(UpdateEvent::Install { version }) => service.install_requested(version),
            },
            _ = poll.tick() => service.poll_files(),
            done = job(&mut service.job) => {
                service.job = None;
                service.checking = false;
                service.finished(done);
                if let Some(version) = service.queued.take() {
                    service.install_requested(version);
                }
            }
        }
    }
}

/// The running job's result; pending without one.
async fn job(job: &mut Option<JoinHandle<Done>>) -> Done {
    match job {
        Some(handle) => match handle.await {
            Ok(done) => done,
            Err(e) => Done::Install(Err(format!("the update task failed: {e}"))),
        },
        None => std::future::pending().await,
    }
}

impl Service {
    /// The marker the helper waits for, and the permissions a new version must keep.
    fn registered(&mut self, perms: Perms) {
        self.perms = perms;
        let marker = Marker {
            version: self.updater.running.clone(),
            pid: std::process::id(),
            at_ms: now_ms(),
            perms,
            service: target::service_label(),
        };
        if let Err(e) = self.updater.dir.write_marker(&marker) {
            warn!("writing the registered marker: {e:#}");
        }
    }

    fn start_check(&mut self) {
        self.checking = true;
        let updater = self.updater.clone();
        self.job = Some(tokio::task::spawn_blocking(move || {
            Done::Check(updater.fetch_manifest())
        }));
    }

    fn install_requested(&mut self, version: String) {
        // Two clicks in the portal can send `update` twice: the second is ignored while an
        // update runs (here, or from `rd-agent update`).
        match &self.job {
            Some(_) if self.checking => {
                info!(
                    version,
                    "update requested during a check; starting after it"
                );
                self.queued = Some(version);
                return;
            }
            Some(_) => {
                info!(
                    version,
                    "update requested while one is in progress; ignored"
                );
                return;
            }
            None if self.updater.dir.active_lock(now_ms()).is_some() => {
                info!(
                    version,
                    "update requested while one is in progress; ignored"
                );
                return;
            }
            None => {}
        }
        let target = match &self.target {
            Ok(target) => target.clone(),
            Err(reason) => {
                return self.refuse(format!("this agent cannot update itself: {reason}"));
            }
        };
        info!(version, "installing an update");
        self.status.send_modify(|s| {
            s.phase = UpdatePhase::Downloading;
            s.error.clear();
        });
        let install = Install {
            updater: self.updater.clone(),
            status: self.status.clone(),
            sessions: self.sessions.clone(),
            target,
            perms: self.perms,
            version,
        };
        self.job = Some(tokio::spawn(
            async move { Done::Install(install.run().await) },
        ));
    }

    fn refuse(&self, error: String) {
        warn!("{error}");
        self.status.send_modify(|s| s.error = error);
    }

    fn finished(&mut self, done: Done) {
        match done {
            Done::Check(Ok(manifest)) => {
                let available = manifest.newer_than(&self.updater.running_version());
                match &available {
                    Some(a) => info!(version = a.version, "an update is available"),
                    None => info!("rd-agent is up to date"),
                }
                self.status.send_modify(|s| {
                    s.available = available;
                    s.checked_ms = Some(now_ms());
                    s.error.clear();
                });
            }
            Done::Check(Err(e)) => {
                warn!("checking for updates failed: {e:#}");
                self.status
                    .send_modify(|s| s.error = format!("checking for updates failed: {e:#}"));
            }
            Done::Install(Ok(outcome)) => {
                info!(result = ?outcome.result, error = outcome.error, "the update ended");
                self.status.send_modify(|s| {
                    s.phase = UpdatePhase::Idle;
                    s.last = Some(outcome);
                });
            }
            Done::Install(Err(e)) => {
                error!("the update failed: {e}");
                self.status.send_modify(|s| {
                    s.phase = UpdatePhase::Idle;
                    s.error = e;
                });
            }
        }
        self.poll_files();
    }

    /// The outcome of the last update (the helper writes it, possibly after this process
    /// started), and whether an update is in progress (e.g. one started from a terminal).
    fn poll_files(&self) {
        let dir = &self.updater.dir;
        let last = dir.read_outcome();
        let busy = dir.active_lock(now_ms()).is_some();
        let idle = self.job.is_none();
        self.status.send_if_modified(|s| {
            let mut changed = false;
            if s.last != last && last.is_some() {
                s.last = last;
                changed = true;
            }
            let phase = if busy {
                UpdatePhase::Installing
            } else {
                UpdatePhase::Idle
            };
            if idle && s.phase != phase {
                s.phase = phase;
                changed = true;
            }
            changed
        });
    }
}

/// One update, from the gateway's `update` until the helper has it (or it failed first).
struct Install {
    updater: Arc<Updater>,
    status: watch::Sender<UpdateStatus>,
    sessions: SessionControl,
    target: Target,
    perms: Perms,
    version: String,
}

impl Install {
    async fn run(self) -> Result<UpdateOutcome, String> {
        let updater = self.updater.clone();
        let from = updater.running.clone();
        let started = now_ms();
        {
            let (u, from, to) = (updater.clone(), from.clone(), self.version.clone());
            blocking(move || u.dir.take_lock(&from, &to, started)).await?;
        }
        let result = self.hand_over(started).await;
        if result.is_err() {
            updater.dir.release_lock();
        }
        result
    }

    async fn hand_over(&self, started: u64) -> Result<UpdateOutcome, String> {
        let (updater, target, perms, version) = (
            self.updater.clone(),
            self.target.clone(),
            self.perms,
            self.version.clone(),
        );
        let plan = blocking(move || {
            let manifest = updater.manifest_for(&version)?;
            updater.prepare(&manifest, &target, perms, None)
        })
        .await?;

        // From here on the agent goes away: end the session (viewers see 4010 and keep
        // reconnecting) and turn new ones away until this process is replaced.
        let _hold = self.sessions.hold(SESSION_END_WAIT).await;
        let updater = self.updater.clone();
        let start = plan.clone();
        blocking(move || updater.start_helper(&start)).await?;
        self.status
            .send_modify(|s| s.phase = UpdatePhase::Installing);
        info!(to = plan.to, "the update helper took over");

        // Normally this process is stopped (and replaced) by the helper. It is still here
        // when the update failed before the switch, or the helper died.
        let dir = self.updater.dir.clone();
        let handed_over = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Some(outcome) = dir
                .read_outcome()
                .filter(|o| o.from == plan.from && o.to == plan.to && o.at_ms >= started)
            {
                return Ok(outcome);
            }
            match dir.active_lock(now_ms()) {
                None => {
                    return Err(
                        "the update helper ended without a result (see update-helper.log)".into(),
                    );
                }
                Some(lock) if lock.helper_pid.is_none() && handed_over.elapsed() > HELPER_START => {
                    return Err("the update helper did not start (see update-helper.log)".into());
                }
                Some(_) => {}
            }
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("the update task failed: {e}")),
    }
}
