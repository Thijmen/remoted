//! macOS: [`MacPlatform`], Keychain, TCC permissions, LaunchAgent, selftest.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use rd_agent_core::{Perms, Platform};
use rd_platform_macos::selftest::SelftestOptions;
use rd_platform_macos::{MacPlatform, RateControl, VideoSource, keychain, session};

use crate::{SelftestArgs, Source};

/// Bundle id; also the unified-log subsystem, the Keychain service and the LaunchAgent label.
const APP_ID: &str = "dev.thijmen.rd.agent";
pub const LOG_SUBSYSTEM: &str = APP_ID;

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `~/Library/Application Support/dev.thijmen.rd.agent/config.toml`
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(home()?
        .join("Library/Application Support")
        .join(APP_ID)
        .join("config.toml"))
}

/// `~/Library/Logs/rd-agent`
pub fn log_dir() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library/Logs/rd-agent"))
}

/// The synthetic source is built into [`MacPlatform`]: it swaps the ScreenCaptureKit pipeline
/// for a test pattern through the same encoder and reports the screen permission as granted.
pub fn platform(source: Source) -> anyhow::Result<Arc<dyn Platform>> {
    let video = match source {
        Source::Screen => VideoSource::Screen,
        Source::Synthetic => VideoSource::Synthetic,
    };
    let platform: Arc<dyn Platform> = MacPlatform::with_options(video, RateControl::from_env())?;
    Ok(platform)
}

pub fn load_token() -> anyhow::Result<Option<String>> {
    keychain::load_token()
}

pub fn store_token(token: &str) -> anyhow::Result<()> {
    keychain::store_token(token)
}

/// Current grants of this process (never prompts).
pub fn permissions() -> anyhow::Result<Perms> {
    Ok(session::permissions())
}

/// Shows the system permission prompts (only `setup`, run in person).
pub fn request_permissions() -> anyhow::Result<Perms> {
    Ok(session::request_permissions())
}

/// Blocks for the duration of the test.
pub fn selftest(args: &SelftestArgs) -> anyhow::Result<bool> {
    let rate_control = match &args.rate_control {
        Some(mode) => mode.parse()?,
        None => RateControl::from_env(),
    };
    let opts = SelftestOptions {
        width: args.size.0,
        height: args.size.1,
        fps: args.fps,
        seconds: args.seconds,
        bitrate_kbps: args.bitrate_kbps,
        record: args.record.clone(),
        synthetic: args.synthetic,
        rate_control,
    };
    rd_platform_macos::selftest::run(&opts)
}

/// The per-user LaunchAgent (`~/Library/LaunchAgents/dev.thijmen.rd.agent.plist`), same
/// steps as packaging/macos/install.sh.
pub mod service {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use anyhow::{Context as _, bail, ensure};
    use tracing::warn;

    use super::{APP_ID, home, log_dir};
    use crate::host::ServiceStatus;

    const LABEL: &str = APP_ID;
    /// `packaging/macos/dev.thijmen.rd.agent.plist.template` or the crate's fallback copy
    /// (chosen by build.rs). Placeholders: `@HOME@`, `@PROGRAM@` (this executable).
    const TEMPLATE: &str = include_str!(concat!(
        env!("OUT_DIR"),
        "/dev.thijmen.rd.agent.plist.template"
    ));
    const LAUNCHCTL: &str = "/bin/launchctl";
    /// How long `install` waits for launchd to let go of the previous instance.
    const BOOTOUT_WAIT: Duration = Duration::from_secs(5);

    pub fn plist_path() -> anyhow::Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// Writes the plist and (re)loads it into the GUI session.
    pub fn install() -> anyhow::Result<()> {
        let program = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("locating the rd-agent executable")?;
        let home = home()?;
        let plist = render(TEMPLATE, &program, &home)?;
        if !plist.contains(&format!(
            "<string>{}</string>",
            xml_escape(&program.to_string_lossy())
        )) {
            warn!(
                "the LaunchAgent template starts a fixed program, not this executable ({}); \
                 install the app (packaging/macos/install.sh) for it to run",
                program.display()
            );
        }
        let path = plist_path()?;
        for dir in [path.parent().map(Path::to_path_buf), Some(log_dir()?)]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

        let domain = gui_domain();
        let target = format!("{domain}/{LABEL}");
        // Stop a running instance and wait until launchd has let go of it.
        if launchctl(&["bootout", &target]).is_ok() {
            let deadline = std::time::Instant::now() + BOOTOUT_WAIT;
            while is_loaded(&target)? && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let path_str = path.to_str().context("the LaunchAgent path is not UTF-8")?;
        launchctl(&["enable", &target])?;
        launchctl(&["bootstrap", &domain, path_str])?;
        println!("LaunchAgent installed: {}", path.display());
        println!("loaded into {domain}; restart with `launchctl kickstart -k {target}`");
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<()> {
        match launchctl(&["bootout", &format!("{}/{LABEL}", gui_domain())]) {
            Ok(()) => println!("LaunchAgent unloaded"),
            Err(e) => println!("LaunchAgent was not loaded ({e:#})"),
        }
        let path = plist_path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
        }
        Ok(())
    }

    pub fn status() -> anyhow::Result<ServiceStatus> {
        let path = plist_path()?;
        let output = Command::new(LAUNCHCTL)
            .args(["print", &format!("{}/{LABEL}", gui_domain())])
            .output()
            .context("running launchctl")?;
        let (state, pid) = if output.status.success() {
            parse_print(&String::from_utf8_lossy(&output.stdout))
        } else {
            (None, None)
        };
        Ok(ServiceStatus {
            installed: path.is_file().then_some(path),
            loaded: output.status.success(),
            state,
            pid,
        })
    }

    fn gui_domain() -> String {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}")
    }

    fn is_loaded(target: &str) -> anyhow::Result<bool> {
        let status = Command::new(LAUNCHCTL)
            .args(["print", target])
            .output()
            .context("running launchctl")?
            .status;
        Ok(status.success())
    }

    fn launchctl(args: &[&str]) -> anyhow::Result<()> {
        let output = Command::new(LAUNCHCTL)
            .args(args)
            .output()
            .context("running launchctl")?;
        if !output.status.success() {
            bail!(
                "launchctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn render(template: &str, program: &Path, home: &Path) -> anyhow::Result<String> {
        let utf8 = |p: &Path| {
            p.to_str()
                .map(xml_escape)
                .with_context(|| format!("{} is not valid UTF-8", p.display()))
        };
        let plist = template
            .replace("@PROGRAM@", &utf8(program)?)
            .replace("@HOME@", &utf8(home)?);
        if let Some(unknown) = placeholder(&plist) {
            bail!("the LaunchAgent template has an unknown placeholder {unknown}");
        }
        ensure!(
            plist.contains(LABEL),
            "the LaunchAgent template does not use the label {LABEL}"
        );
        Ok(plist)
    }

    /// The first `@NAME@` (uppercase letters and `_`) left in `s`.
    fn placeholder(s: &str) -> Option<&str> {
        s.match_indices('@').find_map(|(i, _)| {
            let rest = &s[i + 1..];
            let end = rest.find(|c: char| !(c.is_ascii_uppercase() || c == '_'))?;
            (end > 0 && rest[end..].starts_with('@')).then(|| &s[i..i + end + 2])
        })
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// `state` and `pid` from `launchctl print gui/<uid>/<label>`.
    fn parse_print(out: &str) -> (Option<String>, Option<u32>) {
        let field = |name: &str| {
            out.lines().find_map(|line| {
                let (key, value) = line.trim().split_once(" = ")?;
                (key == name).then(|| value.trim().to_owned())
            })
        };
        (field("state"), field("pid").and_then(|p| p.parse().ok()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const FALLBACK: &str = include_str!("../../assets/dev.thijmen.rd.agent.plist.template");

        #[test]
        fn renders_the_templates() {
            let home = Path::new("/Users/me");
            let program = Path::new("/opt/R&D/rd-agent");
            // Whichever template the build picked.
            let plist = render(TEMPLATE, program, home).unwrap();
            assert!(plist.contains("<string>dev.thijmen.rd.agent</string>"));
            assert!(plist.contains("<string>run</string>"));
            assert!(placeholder(&plist).is_none());
            let plist = render(FALLBACK, program, home).unwrap();
            assert!(plist.contains("<string>/opt/R&amp;D/rd-agent</string>"));
            assert!(plist.contains("<string>/Users/me/Library/Logs/rd-agent/launchd.log</string>"));
            assert!(render("dev.thijmen.rd.agent @OTHER@", program, home).is_err());
            assert!(render("no label", program, home).is_err());
        }

        #[test]
        fn finds_placeholders() {
            assert_eq!(placeholder("a @HOME_DIR@ b"), Some("@HOME_DIR@"));
            assert_eq!(placeholder("me@example.com @@ @x@"), None);
        }

        #[test]
        fn parses_launchctl_print() {
            let out = "gui/501/dev.thijmen.rd.agent = {\n\tactive count = 1\n\tstate = running\n\n\tpid = 4242\n\tprogram = /x\n}";
            assert_eq!(parse_print(out), (Some("running".into()), Some(4242)));
            assert_eq!(parse_print("nothing"), (None, None));
        }
    }
}
