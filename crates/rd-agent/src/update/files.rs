//! The update directory, next to config.toml (macOS `~/Library/Application
//! Support/dev.thijmen.rd.agent/update`, Windows `%LOCALAPPDATA%\rd-agent\update`). Every
//! version uses the same place, because the old and the new version work together through it:
//!
//! ```text
//! registered.json  written by every `rd-agent run` once the gateway accepted it (Marker)
//! outcome.json     how the last update ended, written by the helper (UpdateOutcome)
//! helper.lock      an update is in progress (Lock); free once its helper died, or after
//!                  LOCK_STALE
//! plan.json        what the helper is to do (helper::Plan)
//! previous/        a copy of the version that started the update; the helper runs from it
//! download/        the new version's file (and on macOS the unpacked app)
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use rd_proto::control::{Perms, UpdateOutcome};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// An update that has not finished after this long is considered dead (the helper's own
/// limits add up to less).
pub const LOCK_STALE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateDir {
    root: PathBuf,
}

/// Written by `rd-agent run` after every `registered` from the gateway: the helper's sign that
/// a new version works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub version: String,
    pub pid: u32,
    /// Unix ms.
    pub at_ms: u64,
    pub perms: Perms,
    /// macOS: the LaunchAgent that runs it, so `rd-agent update` restarts the right one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// An update in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub from: String,
    pub to: String,
    /// When it started, Unix ms.
    pub at_ms: u64,
    /// The helper, once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_pid: Option<u32>,
}

impl UpdateDir {
    /// `update/` next to the config file.
    pub fn for_config(config: &Path) -> UpdateDir {
        let dir = config.parent().unwrap_or(Path::new("."));
        UpdateDir {
            root: dir.join("update"),
        }
    }

    pub fn new(root: PathBuf) -> UpdateDir {
        UpdateDir { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn marker(&self) -> PathBuf {
        self.root.join("registered.json")
    }

    pub fn outcome(&self) -> PathBuf {
        self.root.join("outcome.json")
    }

    pub fn lock(&self) -> PathBuf {
        self.root.join("helper.lock")
    }

    pub fn plan(&self) -> PathBuf {
        self.root.join("plan.json")
    }

    pub fn previous(&self) -> PathBuf {
        self.root.join("previous")
    }

    pub fn download(&self) -> PathBuf {
        self.root.join("download")
    }

    pub fn create(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))
    }

    pub fn read_marker(&self) -> Option<Marker> {
        read_json(&self.marker())
    }

    pub fn write_marker(&self, marker: &Marker) -> anyhow::Result<()> {
        self.create()?;
        write_json(&self.marker(), marker)
    }

    pub fn read_outcome(&self) -> Option<UpdateOutcome> {
        read_json(&self.outcome())
    }

    pub fn write_outcome(&self, outcome: &UpdateOutcome) -> anyhow::Result<()> {
        self.create()?;
        write_json(&self.outcome(), outcome)
    }

    /// The update in progress, if any (a stale lock counts as none).
    pub fn active_lock(&self, now_ms: u64) -> Option<Lock> {
        self.active_lock_with(now_ms, super::helper::process_alive)
    }

    fn active_lock_with(&self, now_ms: u64, alive: impl Fn(u32) -> bool) -> Option<Lock> {
        let lock: Lock = read_json(&self.lock())?;
        let fresh = now_ms.saturating_sub(lock.at_ms) < LOCK_STALE.as_millis() as u64;
        // Before the helper runs, the agent itself is downloading (fresh is enough).
        let helper_alive = lock.helper_pid.is_none_or(alive);
        (fresh && helper_alive).then_some(lock)
    }

    /// The helper records itself in the lock, so that its death frees it.
    pub fn set_helper_pid(&self, pid: u32) -> anyhow::Result<()> {
        let mut lock: Lock = read_json(&self.lock()).context("the update lock is gone")?;
        lock.helper_pid = Some(pid);
        write_json(&self.lock(), &lock)
    }

    /// Takes the lock for an update `from` -> `to`. Fails while another update is in progress.
    pub fn take_lock(&self, from: &str, to: &str, now_ms: u64) -> anyhow::Result<()> {
        if let Some(lock) = self.active_lock(now_ms) {
            anyhow::bail!(
                "an update to {} is already in progress (started {} s ago)",
                lock.to,
                now_ms.saturating_sub(lock.at_ms) / 1000
            );
        }
        self.create()?;
        write_json(
            &self.lock(),
            &Lock {
                from: from.to_owned(),
                to: to.to_owned(),
                at_ms: now_ms,
                helper_pid: None,
            },
        )
    }

    pub fn release_lock(&self) {
        let _ = std::fs::remove_file(self.lock());
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Written to a temporary file and renamed, so readers never see half a file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use rd_proto::control::UpdateResult;

    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "rd-agent-update-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lives_next_to_the_config_file() {
        let dir = UpdateDir::for_config(Path::new("/x/dev.thijmen.rd.agent/config.toml"));
        assert_eq!(dir.root(), Path::new("/x/dev.thijmen.rd.agent/update"));
        assert_eq!(
            dir.marker(),
            Path::new("/x/dev.thijmen.rd.agent/update/registered.json")
        );
    }

    #[test]
    fn markers_outcomes_and_locks() {
        let scratch = Scratch::new("files");
        let dir = UpdateDir::new(scratch.0.join("update"));
        assert_eq!(dir.read_marker(), None);
        let marker = Marker {
            version: "1.5.0".into(),
            pid: 42,
            at_ms: 1000,
            perms: Perms {
                screen: true,
                input: false,
            },
            service: Some("dev.thijmen.rd.agent".into()),
        };
        dir.write_marker(&marker).unwrap();
        assert_eq!(dir.read_marker(), Some(marker));

        let outcome = UpdateOutcome {
            from: "1.5.0".into(),
            to: "1.6.0".into(),
            result: UpdateResult::RolledBack,
            error: "1.6.0 did not reconnect within 120 s".into(),
            at_ms: 5,
        };
        dir.write_outcome(&outcome).unwrap();
        assert_eq!(dir.read_outcome(), Some(outcome));
        // A broken file reads as none.
        std::fs::write(dir.outcome(), b"{").unwrap();
        assert_eq!(dir.read_outcome(), None);

        let now = 10_000_000;
        dir.take_lock("1.5.0", "1.6.0", now).unwrap();
        assert!(dir.active_lock(now + 1000).is_some());
        let err = dir.take_lock("1.5.0", "1.6.0", now + 1000).unwrap_err();
        assert!(err.to_string().contains("already in progress"), "{err}");
        // Stale: a helper that died does not block updates forever.
        let later = now + LOCK_STALE.as_millis() as u64;
        assert!(dir.active_lock(later).is_none());
        dir.take_lock("1.5.0", "1.6.0", later).unwrap();
        dir.release_lock();
        assert!(dir.active_lock(later).is_none());

        // Once the helper runs, the lock lives as long as it does.
        dir.take_lock("1.5.0", "1.6.0", now).unwrap();
        dir.set_helper_pid(4242).unwrap();
        let lock = dir.active_lock_with(now, |pid| pid == 4242).unwrap();
        assert_eq!(lock.helper_pid, Some(4242));
        assert!(dir.active_lock_with(now, |_| false).is_none());
        dir.release_lock();
        assert!(dir.set_helper_pid(1).is_err());
    }
}
