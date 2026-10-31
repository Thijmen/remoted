//! Linux: no screen. The agent serves the terminal and files ([`HeadlessPlatform`]); no
//! permissions to grant; a systemd user unit starts it (docs/setup-linux.md).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use rd_agent_core::headless::HeadlessPlatform;
use rd_agent_core::{Perms, Platform};

use crate::{SelftestArgs, Source};

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `$var` when it is an absolute path, else `~/<fallback>` (the XDG base directories).
fn xdg(var: &str, fallback: &str) -> anyhow::Result<PathBuf> {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => Ok(dir),
        _ => Ok(home()?.join(fallback)),
    }
}

/// `~/.config/rd-agent/config.toml`
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?
        .join("rd-agent")
        .join("config.toml"))
}

/// `~/.local/state/rd-agent/logs`, or `RD_AGENT_LOG_DIR`.
pub fn log_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = super::log_dir_override() {
        return Ok(dir);
    }
    Ok(xdg("XDG_STATE_HOME", ".local/state")?
        .join("rd-agent")
        .join("logs"))
}

pub fn platform(source: Source) -> anyhow::Result<Arc<dyn Platform>> {
    if source == Source::Synthetic {
        bail!("--source synthetic needs a video encoder; on Linux the agent has no screen");
    }
    Ok(Arc::new(HeadlessPlatform))
}

/// No Keychain: the token is in the config file or the environment.
pub fn load_token() -> anyhow::Result<Option<String>> {
    Ok(None)
}

/// Nothing to grant without a screen.
pub fn app_permissions(_request: bool) -> anyhow::Result<Perms> {
    Ok(Perms::default())
}

pub fn report_permissions(
    _request: bool,
    _screen_only: bool,
    _report: &std::path::Path,
) -> anyhow::Result<()> {
    bail!("there are no permissions to grant on Linux")
}

pub fn selftest(_args: &SelftestArgs) -> anyhow::Result<bool> {
    bail!("selftest measures screen capture and encoding; on Linux the agent has no screen")
}

/// The systemd user unit, `~/.config/systemd/user/rd-agent.service`.
pub mod service {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context as _, bail, ensure};

    use super::xdg;
    use crate::host::ServiceStatus;

    const UNIT: &str = "rd-agent.service";
    const TEMPLATE: &str = include_str!("../../../../packaging/linux/rd-agent.service.template");

    pub fn unit_path() -> anyhow::Result<PathBuf> {
        Ok(xdg("XDG_CONFIG_HOME", ".config")?
            .join("systemd/user")
            .join(UNIT))
    }

    /// Writes the unit, then enables and (re)starts it.
    pub fn install() -> anyhow::Result<()> {
        let program = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("locating the rd-agent executable")?;
        let unit = render(TEMPLATE, &program)?;
        let path = unit_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT])?;
        systemctl(&["restart", UNIT])?;
        println!(
            "systemd user unit installed and started: {}",
            path.display()
        );
        println!("logs: journalctl --user -u {UNIT}");
        if !lingering() {
            println!(
                "note: your user services stop when you log out. To keep the agent running, \
                 also at boot: sudo loginctl enable-linger {}",
                user()
            );
        }
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<()> {
        match systemctl(&["disable", "--now", UNIT]) {
            Ok(()) => println!("{UNIT} stopped and disabled"),
            Err(e) => println!("{UNIT} was not enabled ({e:#})"),
        }
        let path = unit_path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
            let _ = systemctl(&["daemon-reload"]);
        }
        Ok(())
    }

    pub fn status() -> anyhow::Result<ServiceStatus> {
        let path = unit_path()?;
        let output = Command::new("systemctl")
            .args(["--user", "show", UNIT])
            .args(["-p", "LoadState,ActiveState,SubState,MainPID"])
            .output()
            .context("running systemctl")?;
        let (loaded, state, pid) = if output.status.success() {
            parse_show(&String::from_utf8_lossy(&output.stdout))
        } else {
            (false, None, None)
        };
        Ok(ServiceStatus {
            installed: path.is_file().then_some(path),
            loaded,
            state,
            pid,
        })
    }

    /// No tray on Linux, so nobody asks; `None` like an agent without a login service.
    pub fn start_at_login() -> anyhow::Result<Option<bool>> {
        Ok(None)
    }

    pub fn set_start_at_login(on: bool) -> anyhow::Result<()> {
        systemctl(&[if on { "enable" } else { "disable" }, UNIT])
    }

    /// No tray to quit from: the agent simply exits (systemd restarts it).
    pub fn stop_from_within() -> bool {
        false
    }

    pub fn start() -> anyhow::Result<()> {
        ensure!(
            unit_path()?.is_file(),
            "the agent is not set up yet: run `rd-agent setup`"
        );
        systemctl(&["start", UNIT])
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        let output = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .context("running systemctl (is this a systemd system?)")?;
        if !output.status.success() {
            bail!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn user() -> String {
        std::env::var("USER").unwrap_or_else(|_| "$USER".into())
    }

    fn lingering() -> bool {
        Command::new("loginctl")
            .args(["show-user", &user(), "-p", "Linger", "--value"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
    }

    /// (loaded, "active (running)", pid) from `systemctl show -p …`.
    fn parse_show(out: &str) -> (bool, Option<String>, Option<u32>) {
        let prop = |name: &str| {
            out.lines().find_map(|line| {
                let (k, v) = line.split_once('=')?;
                (k == name).then(|| v.trim().to_owned())
            })
        };
        let state = match (prop("ActiveState"), prop("SubState")) {
            (Some(a), Some(s)) => Some(format!("{a} ({s})")),
            (a, _) => a,
        };
        (
            prop("LoadState").as_deref() == Some("loaded"),
            state,
            prop("MainPID")
                .and_then(|p| p.parse().ok())
                .filter(|&p| p != 0),
        )
    }

    fn render(template: &str, program: &Path) -> anyhow::Result<String> {
        let program = program
            .to_str()
            .with_context(|| format!("{} is not valid UTF-8", program.display()))?;
        // systemd would read these as quoting, escapes or specifiers.
        ensure!(
            !program.contains(['"', '\'', '\\', '\n', '%', '$']),
            "the rd-agent path {program:?} has characters systemd would misread; move the binary"
        );
        let program = if program.contains(' ') {
            format!("\"{program}\"")
        } else {
            program.to_owned()
        };
        let unit = template.replace("@PROGRAM@", &program);
        ensure!(
            !unit.contains("@PROGRAM@"),
            "the unit template has no @PROGRAM@"
        );
        Ok(unit)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn renders_the_unit() {
            let unit = render(TEMPLATE, Path::new("/home/me/.local/bin/rd-agent")).unwrap();
            assert!(
                unit.contains("\nExecStart=/home/me/.local/bin/rd-agent run\n"),
                "{unit}"
            );
            let unit = render(TEMPLATE, Path::new("/opt/rd agent/rd-agent")).unwrap();
            assert!(
                unit.contains("\nExecStart=\"/opt/rd agent/rd-agent\" run\n"),
                "{unit}"
            );
            assert!(render(TEMPLATE, Path::new("/x/100%/rd-agent")).is_err());
        }

        #[test]
        fn parses_systemctl_show() {
            let out = "MainPID=4242\nLoadState=loaded\nActiveState=active\nSubState=running\n";
            assert_eq!(
                parse_show(out),
                (true, Some("active (running)".into()), Some(4242))
            );
            let out = "MainPID=0\nLoadState=not-found\nActiveState=inactive\nSubState=dead\n";
            assert_eq!(
                parse_show(out),
                (false, Some("inactive (dead)".into()), None)
            );
        }
    }
}
