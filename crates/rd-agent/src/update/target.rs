//! Can this agent install updates itself? Only agents installed the normal way can
//! (docs/updates.md): on macOS RD Agent.app started by its LaunchAgent, on Windows the
//! installer's folder started by the autostart. Otherwise the portal is told why.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use super::files::UpdateDir;

/// Where and how this agent is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Mac {
        /// e.g. `/Applications/RD Agent.app`.
        app: PathBuf,
        /// The LaunchAgent that runs it, in `domain` (`gui/<uid>`).
        label: String,
        domain: String,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows {
        /// The folder with rd-agent.exe, rd-agentw.exe and uninstall.exe.
        dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `rd-agent run`: must have been started by the login service.
    Service,
    /// `rd-agent update` from a terminal: updates the installed agent it belongs to.
    Cli,
}

/// What a macOS agent knows about how it was started.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacFacts {
    /// The app bundle this executable is in.
    pub app: Option<PathBuf>,
    /// The LaunchAgent to check: the one that started this process (Service), or the one the
    /// running agent reported (Cli).
    pub label: Option<String>,
    /// What that LaunchAgent starts (`launchctl print`); `None` when it is not loaded.
    pub job_program: Option<PathBuf>,
    /// The app's folder can be written to (the app is replaced by renaming it).
    pub folder_writable: bool,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn check_mac(f: &MacFacts) -> Result<(), String> {
    let Some(app) = &f.app else {
        return Err("not running from RD Agent.app".into());
    };
    if app.to_string_lossy().contains("/AppTranslocation/") {
        return Err(
            "RD Agent.app runs from a temporary copy made by Gatekeeper; move it to \
             /Applications"
                .into(),
        );
    }
    let Some(label) = &f.label else {
        return Err("not started by its LaunchAgent".into());
    };
    let Some(program) = &f.job_program else {
        return Err(format!("the LaunchAgent {label} is not loaded"));
    };
    if *program != app.join("Contents/MacOS/rd-agent") {
        return Err(format!(
            "the LaunchAgent {label} starts {}, not this app",
            program.display()
        ));
    }
    if !f.folder_writable {
        let folder = app.parent().unwrap_or(Path::new("/"));
        return Err(format!(
            "{} cannot be written to, so the app cannot be replaced",
            folder.display()
        ));
    }
    Ok(())
}

/// What a Windows agent knows about how it was installed and started.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WinFacts {
    /// The folder of this executable.
    pub dir: PathBuf,
    pub has_uninstaller: bool,
    pub has_launcher: bool,
    /// Started by rd-agentw.exe (the autostart's launcher).
    pub launched_by_launcher: bool,
    /// The launcher the autostart starts, if it is on.
    pub autostart: Option<PathBuf>,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn check_windows(f: &WinFacts, mode: Mode) -> Result<(), String> {
    if !f.has_uninstaller {
        return Err(
            "not installed with the installer (no uninstall.exe next to rd-agent.exe)".into(),
        );
    }
    if !f.has_launcher {
        return Err("rd-agentw.exe is missing next to rd-agent.exe".into());
    }
    match mode {
        Mode::Service if !f.launched_by_launcher => {
            Err("not started by the autostart (rd-agentw.exe)".into())
        }
        Mode::Cli => match &f.autostart {
            None => Err("the autostart is off (run rd-agent install)".into()),
            Some(launcher) if !same_path(launcher, &f.dir.join("rd-agentw.exe")) => Err(format!(
                "the autostart starts another copy ({})",
                launcher.display()
            )),
            Some(_) => Ok(()),
        },
        Mode::Service => Ok(()),
    }
}

/// Windows paths compare without case and without the `\\?\` prefix.
#[cfg_attr(not(windows), allow(dead_code))]
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// The installed agent this process can update, or why it cannot.
#[cfg(target_os = "macos")]
pub fn detect(mode: Mode, dir: &UpdateDir) -> Result<Target, String> {
    let app = crate::host::app_bundle();
    let label = match mode {
        Mode::Service => mac::own_label(),
        // The LaunchAgent the running agent reported in its marker.
        Mode::Cli => mac::label_override()
            .or_else(|| dir.read_marker().and_then(|m| m.service))
            .or_else(|| Some(crate::host::APP_ID.to_owned())),
    };
    let domain = mac::gui_domain();
    let facts = MacFacts {
        // Canonical, like the app's path (`/tmp` is `/private/tmp`, for one).
        job_program: label
            .as_deref()
            .and_then(|label| mac::job_program(&domain, label))
            .map(|p| p.canonicalize().unwrap_or(p)),
        folder_writable: app
            .as_deref()
            .and_then(Path::parent)
            .is_some_and(mac::writable),
        app,
        label,
    };
    check_mac(&facts)?;
    Ok(Target::Mac {
        app: facts.app.expect("checked"),
        label: facts.label.expect("checked"),
        domain,
    })
}

#[cfg(windows)]
pub fn detect(mode: Mode, _dir: &super::files::UpdateDir) -> Result<Target, String> {
    use crate::host::names;

    let exe = std::env::current_exe().map_err(|e| format!("locating rd-agent.exe: {e}"))?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let facts = WinFacts {
        has_uninstaller: dir.join("uninstall.exe").is_file(),
        has_launcher: dir.join(names::LAUNCHER_EXE).is_file(),
        launched_by_launcher: std::env::var_os(names::LAUNCHER_ENV).is_some(),
        autostart: crate::host::service::status()
            .ok()
            .and_then(|s| s.installed),
        dir,
    };
    check_windows(&facts, mode)?;
    Ok(Target::Windows { dir: facts.dir })
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn detect(_mode: Mode, _dir: &super::files::UpdateDir) -> Result<Target, String> {
    Err("updates are for macOS and Windows".into())
}

/// The LaunchAgent label of `rd-agent run`, from launchd's `XPC_SERVICE_NAME`; recorded in
/// the registered marker so that `rd-agent update` finds the job.
pub fn service_label() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        mac::own_label()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
pub mod mac {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Debug builds only: the LaunchAgent `rd-agent update` updates (tests use their own).
    #[cfg(debug_assertions)]
    const LABEL_ENV: &str = "RD_AGENT_SERVICE_LABEL";

    pub fn label_override() -> Option<String> {
        #[cfg(debug_assertions)]
        if let Some(label) = std::env::var(LABEL_ENV).ok().filter(|l| !l.is_empty()) {
            return Some(label);
        }
        None
    }

    /// The label of the launchd job that started this process. launchd sets
    /// `XPC_SERVICE_NAME` to it; apps opened by LaunchServices get `application.…` and a
    /// terminal `0`, which are not LaunchAgents.
    pub fn own_label() -> Option<String> {
        let name = std::env::var("XPC_SERVICE_NAME").ok()?;
        (name.contains('.') && !name.starts_with("application.")).then_some(name)
    }

    pub fn gui_domain() -> String {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}")
    }

    /// The program of a loaded launchd job (read-only `launchctl print`).
    pub fn job_program(domain: &str, label: &str) -> Option<PathBuf> {
        let output = Command::new("/bin/launchctl")
            .args(["print", &format!("{domain}/{label}")])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        print_field(&String::from_utf8_lossy(&output.stdout), "program").map(PathBuf::from)
    }

    /// A top-level field of `launchctl print` (one tab deep; nested blocks have more).
    pub fn print_field(out: &str, name: &str) -> Option<String> {
        out.lines().find_map(|line| {
            let rest = line.strip_prefix('\t')?;
            if rest.starts_with('\t') {
                return None;
            }
            let (key, value) = rest.split_once(" = ")?;
            (key == name).then(|| value.trim().to_owned())
        })
    }

    pub fn writable(dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a NUL-terminated path.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_launchctl_print() {
            let out = "gui/501/dev.thijmen.rd.agent = {\n\tactive count = 1\n\tpath = /Users/me/Library/LaunchAgents/dev.thijmen.rd.agent.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Applications/RD Agent.app/Contents/MacOS/rd-agent\n\targuments = {\n\t\t/Applications/RD Agent.app/Contents/MacOS/rd-agent\n\t\trun\n\t}\n\n\tendpoints = {\n\t\tprogram = /elsewhere\n\t}\n}\n";
            assert_eq!(
                print_field(out, "program").as_deref(),
                Some("/Applications/RD Agent.app/Contents/MacOS/rd-agent")
            );
            assert_eq!(print_field(out, "state").as_deref(), Some("running"));
            assert_eq!(print_field(out, "pid"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_ok() -> MacFacts {
        MacFacts {
            app: Some("/Applications/RD Agent.app".into()),
            label: Some("dev.thijmen.rd.agent".into()),
            job_program: Some("/Applications/RD Agent.app/Contents/MacOS/rd-agent".into()),
            folder_writable: true,
        }
    }

    #[test]
    fn mac_installs_only_the_app_its_launch_agent_starts() {
        assert_eq!(check_mac(&mac_ok()), Ok(()));
        let reason = |f: MacFacts| check_mac(&f).unwrap_err();
        assert_eq!(
            reason(MacFacts {
                app: None,
                ..mac_ok()
            }),
            "not running from RD Agent.app"
        );
        assert!(
            reason(MacFacts {
                app: Some("/private/var/folders/x/AppTranslocation/y/d/RD Agent.app".into()),
                ..mac_ok()
            })
            .contains("Gatekeeper")
        );
        assert_eq!(
            reason(MacFacts {
                label: None,
                ..mac_ok()
            }),
            "not started by its LaunchAgent"
        );
        assert_eq!(
            reason(MacFacts {
                job_program: None,
                ..mac_ok()
            }),
            "the LaunchAgent dev.thijmen.rd.agent is not loaded"
        );
        assert!(
            reason(MacFacts {
                job_program: Some("/Users/me/rd-agent".into()),
                ..mac_ok()
            })
            .contains("starts /Users/me/rd-agent, not this app")
        );
        assert!(
            reason(MacFacts {
                folder_writable: false,
                ..mac_ok()
            })
            .starts_with("/Applications cannot be written to")
        );
    }

    fn win_ok() -> WinFacts {
        WinFacts {
            dir: r"C:\Users\me\AppData\Local\Programs\rd-agent".into(),
            has_uninstaller: true,
            has_launcher: true,
            launched_by_launcher: true,
            autostart: Some(r"C:\Users\Me\AppData\Local\Programs\rd-agent\rd-agentw.exe".into()),
        }
    }

    #[test]
    fn windows_installs_only_the_installer_s_folder() {
        assert_eq!(check_windows(&win_ok(), Mode::Service), Ok(()));
        assert_eq!(check_windows(&win_ok(), Mode::Cli), Ok(()));
        let portable = WinFacts {
            has_uninstaller: false,
            ..win_ok()
        };
        assert!(
            check_windows(&portable, Mode::Service)
                .unwrap_err()
                .starts_with("not installed with the installer")
        );
        let by_hand = WinFacts {
            launched_by_launcher: false,
            ..win_ok()
        };
        assert_eq!(
            check_windows(&by_hand, Mode::Service).unwrap_err(),
            "not started by the autostart (rd-agentw.exe)"
        );
        // From a terminal: the autostart must start this folder's launcher.
        assert_eq!(check_windows(&by_hand, Mode::Cli), Ok(()));
        let elsewhere = WinFacts {
            autostart: Some(r"D:\rd\rd-agentw.exe".into()),
            ..win_ok()
        };
        assert!(
            check_windows(&elsewhere, Mode::Cli)
                .unwrap_err()
                .contains("another copy")
        );
        let off = WinFacts {
            autostart: None,
            ..win_ok()
        };
        assert!(check_windows(&off, Mode::Cli).is_err());
    }
}
