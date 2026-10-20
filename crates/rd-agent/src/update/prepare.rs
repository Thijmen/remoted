//! Everything before the helper takes over, shared by `rd-agent run` (the portal's Update
//! button) and `rd-agent update`: read the manifest, download the file, check its signature,
//! check the new version (macOS: unpack the app, compare its code signature with the running
//! one), copy the running version aside, write the plan and start the helper.
//!
//! Blocking: callers run it off the async runtime.

use std::path::PathBuf;

use anyhow::{Context as _, ensure};
use rd_proto::control::Perms;
use tracing::info;

use super::fetch::Http;
use super::files::UpdateDir;
use super::helper::{Install, Limits, Plan};
use super::manifest::{self, Manifest};
use super::signature;
use super::target::Target;
use super::version::Version;

pub struct Updater {
    /// This build's version.
    pub running: String,
    pub dir: UpdateDir,
    pub log_dir: PathBuf,
    pub manifest_url: String,
    http: Http,
}

impl Updater {
    pub fn new(running: &str, dir: UpdateDir, log_dir: PathBuf) -> Updater {
        let manifest_url = std::env::var(manifest::URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| manifest::DEFAULT_URL.to_owned());
        let os = if cfg!(windows) { "windows" } else { "macos" };
        Updater {
            running: running.to_owned(),
            dir,
            log_dir,
            manifest_url,
            http: Http::new(&format!("rd-agent/{running} ({os})")),
        }
    }

    pub fn running_version(&self) -> Version {
        // The build's own version always parses (checked by a test).
        self.running.parse().expect("this build's version")
    }

    pub fn fetch_manifest(&self) -> anyhow::Result<Manifest> {
        self.http.manifest(&self.manifest_url)
    }

    /// Reads the manifest again and returns it if it offers exactly `version`, newer than this
    /// one. The gateway only asks for versions this agent reported; the manifest may have
    /// moved on since.
    pub fn manifest_for(&self, version: &str) -> anyhow::Result<Manifest> {
        let manifest = self.fetch_manifest()?;
        let wanted: Version = version
            .parse()
            .with_context(|| format!("{version:?} is not a version"))?;
        let latest = manifest.latest();
        ensure!(
            latest == wanted,
            "{version} is not the latest version any more ({latest} is); update again"
        );
        ensure!(
            latest > self.running_version(),
            "{version} is not newer than this agent ({})",
            self.running
        );
        Ok(manifest)
    }

    /// Downloads and checks `manifest`'s version for `target` and writes the helper's plan.
    /// On failure nothing is left in `download/`.
    pub fn prepare(
        &self,
        manifest: &Manifest,
        target: &Target,
        perms: Perms,
        wait_pid: Option<u32>,
    ) -> anyhow::Result<Plan> {
        self.prepare_files(manifest, target, perms, wait_pid)
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(self.dir.download());
            })
    }

    fn prepare_files(
        &self,
        manifest: &Manifest,
        target: &Target,
        perms: Perms,
        wait_pid: Option<u32>,
    ) -> anyhow::Result<Plan> {
        let to = manifest.version.clone();
        let file = manifest.file()?;
        self.dir.create()?;
        let download = self.dir.download();
        let _ = std::fs::remove_dir_all(&download);
        std::fs::create_dir_all(&download)
            .with_context(|| format!("creating {}", download.display()))?;
        let path = download.join(file_name(&file.url));
        info!(url = file.url, path = %path.display(), "downloading the update");
        let size = self.http.download(&file.url, &path)?;
        signature::verify_file(&path, &file.signature, &to)
            .with_context(|| format!("checking {}", path.display()))?;
        info!(size, "update downloaded and its signature checked");

        let previous = self.dir.previous();
        let _ = std::fs::remove_dir_all(&previous);
        std::fs::create_dir_all(&previous)
            .with_context(|| format!("creating {}", previous.display()))?;
        let install = match target {
            Target::Mac { app, label, domain } => {
                let new_app = mac::unpack(&path, &download.join("unpacked"))?;
                mac::check_new_app(&new_app, app, &to)?;
                let previous_app = previous.join(app.file_name().context("app name")?);
                mac::copy_app(app, &previous_app)?;
                let helper_label = format!("{label}.update");
                Install::Macos {
                    app: app.clone(),
                    new_app,
                    previous_app,
                    label: label.clone(),
                    domain: domain.clone(),
                    helper_plist: self.dir.root().join(format!("{helper_label}.plist")),
                    helper_label,
                }
            }
            Target::Windows { dir } => {
                for name in ["rd-agent.exe", "rd-agentw.exe"] {
                    std::fs::copy(dir.join(name), previous.join(name))
                        .with_context(|| format!("copying {name} aside"))?;
                }
                Install::Windows {
                    dir: dir.clone(),
                    setup: path,
                    previous,
                }
            }
        };
        let plan = Plan {
            from: self.running.clone(),
            to,
            dir: self.dir.root().to_owned(),
            log_dir: self.log_dir.clone(),
            perms,
            wait_pid,
            limits: limits(),
            install,
        };
        super::files::write_json(&self.dir.plan(), &plan)?;
        Ok(plan)
    }

    /// Starts `rd-agent update-helper` from the copy in `previous/`, outside the agent's
    /// login service.
    pub fn start_helper(&self, plan: &Plan) -> anyhow::Result<()> {
        let plan_path = self.dir.plan();
        match &plan.install {
            Install::Macos {
                previous_app,
                domain,
                helper_label,
                helper_plist,
                ..
            } => mac::start_helper(
                &previous_app.join("Contents/MacOS/rd-agent"),
                &plan_path,
                domain,
                helper_label,
                helper_plist,
                &self.log_dir,
            ),
            Install::Windows { previous, .. } => {
                win::start_helper(&previous.join("rd-agent.exe"), &plan_path)
            }
        }
    }
}

/// The helper's limits; debug builds take a shorter registration wait from
/// `RD_AGENT_UPDATE_WAIT_S` (tests of the rollback).
fn limits() -> Limits {
    #[allow(unused_mut)]
    let mut limits = Limits::default();
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("RD_AGENT_UPDATE_WAIT_S")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        limits.register_s = s;
        limits.rollback_s = s;
    }
    limits
}

/// The last path segment of `url`, if it is a plain file name.
fn file_name(url: &str) -> String {
    let name = url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments()?.next_back().map(str::to_owned))
        .unwrap_or_default();
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if plain { name } else { "update.bin".into() }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod mac {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context as _, bail, ensure};

    /// The bundle id every update must have.
    pub const BUNDLE_ID: &str = "dev.thijmen.rd.agent";

    fn run(program: &str, args: &[&std::ffi::OsStr]) -> anyhow::Result<String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("running {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Unpacks the release zip (`ditto -c -k --keepParent RD Agent.app`) into `into`; returns
    /// the app in it.
    pub fn unpack(zip: &Path, into: &Path) -> anyhow::Result<PathBuf> {
        run(
            "/usr/bin/ditto",
            &[
                "-x".as_ref(),
                "-k".as_ref(),
                zip.as_os_str(),
                into.as_os_str(),
            ],
        )
        .context("unpacking the update")?;
        let apps: Vec<PathBuf> = std::fs::read_dir(into)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "app") && p.is_dir())
            .collect();
        match apps.as_slice() {
            [app] => Ok(app.clone()),
            _ => bail!("the update does not contain exactly one app"),
        }
    }

    fn plist_value(app: &Path, key: &str) -> anyhow::Result<String> {
        let plist = app.join("Contents/Info.plist");
        let value = run(
            "/usr/bin/plutil",
            &[
                "-extract".as_ref(),
                key.as_ref(),
                "raw".as_ref(),
                "-o".as_ref(),
                "-".as_ref(),
                plist.as_os_str(),
            ],
        )
        .with_context(|| format!("reading {key} from {}", plist.display()))?;
        Ok(value.trim().to_owned())
    }

    /// The running app's designated requirement: what macOS ties the Screen Recording and
    /// Accessibility grants to.
    fn designated_requirement(app: &Path) -> anyhow::Result<String> {
        let out = run(
            "/usr/bin/codesign",
            &["-d".as_ref(), "-r-".as_ref(), app.as_os_str()],
        )?;
        if let Some(dr) = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("designated => "))
        {
            return Ok(dr.to_owned());
        }
        // Ad hoc signatures have only an implied requirement (`# designated => cdhash …`),
        // which no other build meets.
        bail!(
            "{} is signed ad hoc, so macOS would not keep its permissions for a new version; \
             install the new version by hand",
            app.display()
        )
    }

    /// The new app must be RD Agent, the expected version, validly signed with the running
    /// app's certificate (so the permissions carry over), and run.
    pub fn check_new_app(new: &Path, current: &Path, version: &str) -> anyhow::Result<()> {
        let id = plist_value(new, "CFBundleIdentifier")?;
        ensure!(id == BUNDLE_ID, "the update is {id}, not {BUNDLE_ID}");
        let short = plist_value(new, "CFBundleShortVersionString")?;
        ensure!(
            short == version,
            "the update's app is version {short}, not {version}"
        );
        let requirement = designated_requirement(current)?;
        let verified = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "-R"])
            .arg(format!("={requirement}"))
            .arg(new)
            .output()
            .context("running codesign")?;
        if !verified.status.success() {
            tracing::warn!(
                requirement,
                stderr = %String::from_utf8_lossy(&verified.stderr).trim(),
                "the new app does not satisfy the running app's designated requirement"
            );
            bail!(
                "the new version is signed with a different certificate; install it by hand at \
                 the Mac and grant the permissions again"
            );
        }
        let exe = new.join("Contents/MacOS/rd-agent");
        let said = run(&exe.to_string_lossy(), &["--version".as_ref()])
            .context("the new version does not start")?;
        ensure!(
            said.trim() == format!("rd-agent {version}"),
            "the new version reports {:?}, not {version}",
            said.trim()
        );
        Ok(())
    }

    /// A copy of the running app: the helper runs from it, and a rollback uses it should the
    /// app moved aside be gone.
    pub fn copy_app(app: &Path, to: &Path) -> anyhow::Result<()> {
        run("/usr/bin/ditto", &[app.as_os_str(), to.as_os_str()])
            .with_context(|| format!("copying {} aside", app.display()))?;
        Ok(())
    }

    /// The helper as a temporary LaunchAgent: its own launchd job, so restarting the agent's
    /// job does not end it. It removes the job itself when it is done.
    pub fn start_helper(
        program: &Path,
        plan: &Path,
        domain: &str,
        label: &str,
        plist: &Path,
        log_dir: &Path,
    ) -> anyhow::Result<()> {
        let text = helper_plist(label, program, plan, &log_dir.join("update-helper.log"))?;
        std::fs::write(plist, text).with_context(|| format!("writing {}", plist.display()))?;
        let target = format!("{domain}/{label}");
        // A job left by an earlier helper that could not remove itself (it is not running:
        // the update lock was free).
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &target])
            .output();
        run(
            "/bin/launchctl",
            &["bootstrap".as_ref(), domain.as_ref(), plist.as_os_str()],
        )
        .context("starting the update helper")?;
        tracing::info!(label, "update helper started");
        Ok(())
    }

    fn helper_plist(
        label: &str,
        program: &Path,
        plan: &Path,
        log: &Path,
    ) -> anyhow::Result<String> {
        let esc = |p: &Path| {
            p.to_str()
                .map(xml_escape)
                .with_context(|| format!("{} is not valid UTF-8", p.display()))
        };
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>update-helper</string>
		<string>--plan</string>
		<string>{plan}</string>
	</array>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
            label = xml_escape(label),
            program = esc(program)?,
            plan = esc(plan)?,
            log = esc(log)?,
        ))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn helper_plists() {
            let plist = helper_plist(
                "dev.thijmen.rd.agent.update",
                Path::new("/x/previous/RD Agent.app/Contents/MacOS/rd-agent"),
                Path::new("/x/R&D/plan.json"),
                Path::new("/logs/update-helper.log"),
            )
            .unwrap();
            assert!(plist.contains("<string>dev.thijmen.rd.agent.update</string>"));
            assert!(plist.contains("<string>/x/R&amp;D/plan.json</string>"));
            assert!(plist.contains("<string>update-helper</string>"));
            // Runs once: no KeepAlive.
            assert!(!plist.contains("KeepAlive"));
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
mod win {
    use std::path::Path;

    /// The helper as a detached process: outside the console and the launcher, and out of
    /// any job object the agent is in (when Windows allows it), so stopping the agent does
    /// not stop it.
    #[cfg(windows)]
    pub fn start_helper(program: &Path, plan: &Path) -> anyhow::Result<()> {
        use std::os::windows::process::CommandExt as _;
        use std::process::{Command, Stdio};

        use anyhow::Context as _;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        /// `ERROR_ACCESS_DENIED`: the job does not allow breaking away.
        const ACCESS_DENIED: i32 = 5;

        // Not in the install folder, which the installer replaces.
        let cwd = plan.parent().unwrap_or(Path::new("."));
        let spawn = |flags: u32| {
            Command::new(program)
                .args(["update-helper", "--plan"])
                .arg(plan)
                .current_dir(cwd)
                .creation_flags(flags)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        };
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        match spawn(base | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(ACCESS_DENIED) => {
                tracing::warn!("cannot leave the job object; starting the helper inside it");
                spawn(base)
                    .map(|_| ())
                    .with_context(|| format!("starting {}", program.display()))
            }
            Err(e) => Err(e).with_context(|| format!("starting {}", program.display())),
        }
    }

    #[cfg(not(windows))]
    pub fn start_helper(_program: &Path, _plan: &Path) -> anyhow::Result<()> {
        anyhow::bail!("the Windows helper starts only on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names() {
        assert_eq!(
            file_name(
                "https://github.com/x/releases/download/agent-v1.6.0/rd-agent-1.6.0-macos-universal.zip"
            ),
            "rd-agent-1.6.0-macos-universal.zip"
        );
        assert_eq!(file_name("https://example.com/a/..%2Fb"), "update.bin");
        assert_eq!(file_name("https://example.com/"), "update.bin");
        assert_eq!(file_name("https://example.com/.hidden"), "update.bin");
    }

    #[test]
    fn this_build_has_a_valid_version() {
        crate::VERSION.parse::<Version>().unwrap();
    }
}
