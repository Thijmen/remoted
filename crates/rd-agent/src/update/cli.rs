//! `rd-agent update [--check]`: at the machine, the same update the portal's button starts.
//! It works whether or not automatic checks are on: it reads the manifest itself.

use std::time::Duration;

use anyhow::bail;
use rd_proto::control::{Perms, UpdateOutcome, UpdateResult};

use super::files::{LOCK_STALE, now_ms};
use super::prepare::Updater;
use super::target::{self, Mode};

pub fn run(updater: &Updater, check_only: bool) -> anyhow::Result<()> {
    println!("rd-agent {}", updater.running);
    if let Some(last) = updater.dir.read_outcome() {
        println!("last update: {}", describe(&last));
    }
    let target = target::detect(Mode::Cli, &updater.dir);
    match &target {
        Ok(_) => println!("updates: can be installed here"),
        Err(reason) => println!("updates: cannot be installed here ({reason})"),
    }
    println!("checking {}", updater.manifest_url);
    let manifest = updater.fetch_manifest()?;
    let Some(available) = manifest.newer_than(&updater.running_version()) else {
        println!("up to date (the newest version is {})", manifest.version);
        return Ok(());
    };
    println!("\nrd-agent {} is available:\n", available.version);
    println!("{}\n", available.notes);
    if check_only {
        return Ok(());
    }
    let target = match target {
        Ok(target) => target,
        Err(reason) => bail!("cannot install it here: {reason}; install it by hand"),
    };

    let started = now_ms();
    updater
        .dir
        .take_lock(&updater.running, &manifest.version, started)?;
    let plan = (|| {
        // The running agent's permissions, from its marker: the new version must keep them.
        let perms = updater
            .dir
            .read_marker()
            .filter(|m| m.version == updater.running)
            .map_or_else(Perms::default, |m| m.perms);
        // On Windows this process keeps rd-agent.exe open: the helper waits for it to end.
        let wait_pid = cfg!(windows).then(std::process::id);
        println!("downloading {}", manifest.version);
        let plan = updater.prepare(&manifest, &target, perms, wait_pid)?;
        updater.start_helper(&plan)?;
        anyhow::Ok(plan)
    })()
    .inspect_err(|_| updater.dir.release_lock())?;

    let log = updater.log_dir.join("update-helper.log");
    println!(
        "the update helper is installing {} (log: {})",
        plan.to,
        log.display()
    );
    if cfg!(windows) {
        println!("`rd-agent update --check` shows the result in a few minutes");
        return Ok(());
    }
    println!("waiting for it (the agent restarts; up to a few minutes)");
    let deadline = std::time::Instant::now() + LOCK_STALE;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(outcome) = updater
            .dir
            .read_outcome()
            .filter(|o| o.to == plan.to && o.at_ms >= started)
        {
            // A rollback is recorded at once and may be amended; wait for the helper to finish.
            if updater.dir.active_lock(now_ms()).is_none() {
                let outcome = updater.dir.read_outcome().unwrap_or(outcome);
                println!("{}", describe(&outcome));
                if outcome.result != UpdateResult::Ok {
                    bail!("the update did not succeed");
                }
                return Ok(());
            }
        }
        if std::time::Instant::now() > deadline {
            bail!("the update helper did not finish; see {}", log.display());
        }
        if updater.dir.active_lock(now_ms()).is_none()
            && updater.dir.read_outcome().is_none_or(|o| o.at_ms < started)
        {
            bail!(
                "the update helper ended without a result; see {}",
                log.display()
            );
        }
    }
}

fn describe(o: &UpdateOutcome) -> String {
    match o.result {
        UpdateResult::Ok => format!("{} -> {}: installed", o.from, o.to),
        UpdateResult::RolledBack => {
            format!("{} -> {}: rolled back ({})", o.from, o.to, o.error)
        }
        UpdateResult::Failed => format!("{} -> {}: failed ({})", o.from, o.to, o.error),
        UpdateResult::Unknown => format!("{} -> {}: {}", o.from, o.to, o.error),
    }
}
