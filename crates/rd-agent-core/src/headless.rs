//! Agents without a screen: they serve the terminal and the file browser, nothing else.
//!
//! - [`HeadlessPlatform`]: a Unix host with no screen at all (Linux servers). Shells run in
//!   [`crate::pty`] terminals; the file browser starts at the home folder and `/`.
//! - [`Screenless`]: any platform with its screen hidden (`[terminal] only = true`). Its
//!   terminal, files and OS details come from the platform it wraps.
//!
//! Either way the agent lists `headless` in `register.caps`, and the gateway refuses viewer
//! sessions to it (close 4011).

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::bail;

use crate::platform::{
    DisplayDesc, Guard, InputInjector, LockState, Perms, Place, Platform, PtyProcess, PtyRequest,
    VideoPipeline,
};

const NO_SCREEN: &str = "this agent has no screen: it serves the terminal and files only";

/// A host without a screen.
#[derive(Debug, Default)]
pub struct HeadlessPlatform;

impl Platform for HeadlessPlatform {
    fn os(&self) -> &'static str {
        std::env::consts::OS
    }

    fn os_description(&self) -> String {
        os_description()
    }

    fn has_screen(&self) -> bool {
        false
    }

    fn displays(&self) -> Vec<DisplayDesc> {
        Vec::new()
    }

    fn native_size(&self, _display: u32) -> Option<(u32, u32)> {
        None
    }

    fn permissions(&self) -> Perms {
        Perms::default()
    }

    fn lock_state(&self) -> LockState {
        LockState::Unknown
    }

    fn keep_awake(&self, _reason: &str) -> Box<dyn Guard> {
        Box::new(())
    }

    fn declare_user_activity(&self) {}

    fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>> {
        bail!(NO_SCREEN)
    }

    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        bail!(NO_SCREEN)
    }

    fn pty_supported(&self) -> bool {
        cfg!(unix)
    }

    fn spawn_pty(&self, req: &PtyRequest) -> io::Result<PtyProcess> {
        #[cfg(unix)]
        {
            crate::pty::spawn(req)
        }
        #[cfg(not(unix))]
        {
            let _ = req;
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// `inner` with its screen hidden: no capture, no input, no display power assertions, but
/// the terminal and files of `inner`.
pub struct Screenless(pub Arc<dyn Platform>);

impl Platform for Screenless {
    fn os(&self) -> &'static str {
        self.0.os()
    }

    fn os_description(&self) -> String {
        self.0.os_description()
    }

    fn has_screen(&self) -> bool {
        false
    }

    fn displays(&self) -> Vec<DisplayDesc> {
        Vec::new()
    }

    fn native_size(&self, _display: u32) -> Option<(u32, u32)> {
        None
    }

    fn permissions(&self) -> Perms {
        Perms::default()
    }

    fn lock_state(&self) -> LockState {
        LockState::Unknown
    }

    fn keep_awake(&self, _reason: &str) -> Box<dyn Guard> {
        Box::new(())
    }

    fn declare_user_activity(&self) {}

    fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>> {
        bail!(NO_SCREEN)
    }

    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        bail!(NO_SCREEN)
    }

    fn file_places(&self) -> Vec<Place> {
        self.0.file_places()
    }

    fn can_trash(&self) -> bool {
        self.0.can_trash()
    }

    fn trash(&self, path: &Path) -> io::Result<()> {
        self.0.trash(path)
    }

    fn pty_supported(&self) -> bool {
        self.0.pty_supported()
    }

    fn spawn_pty(&self, req: &PtyRequest) -> io::Result<PtyProcess> {
        self.0.spawn_pty(req)
    }
}

/// e.g. "Debian GNU/Linux 13 (trixie), Linux 6.12.48" (`/etc/os-release` and `uname`).
fn os_description() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| pretty_name(&text));
    match (pretty, uname()) {
        (Some(p), Some(k)) => format!("{p}, {k}"),
        (Some(p), None) => p,
        (None, Some(k)) => k,
        (None, None) => std::env::consts::OS.to_owned(),
    }
}

fn pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

#[cfg(unix)]
fn uname() -> Option<String> {
    // SAFETY: utsname is plain old data; uname fills it with NUL-terminated strings.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    let field = |f: &[libc::c_char]| {
        // SAFETY: uname NUL-terminates every field within its array.
        unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Some(format!("{} {}", field(&u.sysname), field(&u.release)))
}

#[cfg(not(unix))]
fn uname() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release() {
        let text = "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";
        assert_eq!(
            pretty_name(text).as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(
            pretty_name("PRETTY_NAME=Alpine\n").as_deref(),
            Some("Alpine")
        );
        assert_eq!(pretty_name("ID=x\n"), None);
    }

    #[test]
    fn no_screen() {
        let p = HeadlessPlatform;
        assert!(!p.has_screen());
        assert!(p.displays().is_empty());
        assert!(p.video_pipeline().is_err());
        assert!(p.input_injector().is_err());
        assert_eq!(p.pty_supported(), cfg!(unix));
        assert!(!p.os_description().is_empty());

        let wrapped = Screenless(Arc::new(HeadlessPlatform));
        assert!(!wrapped.has_screen());
        assert_eq!(wrapped.pty_supported(), cfg!(unix));
        assert_eq!(wrapped.os(), std::env::consts::OS);
    }

    #[cfg(unix)]
    #[test]
    fn starts_a_shell() {
        use std::io::{Read as _, Write as _};

        let req = PtyRequest {
            size: crate::platform::PtySize { cols: 90, rows: 30 },
            shell: Some("/bin/sh".into()),
            env: vec![("ENV".into(), "/dev/null".into())],
            env_remove: Vec::new(),
        };
        let mut pty = HeadlessPlatform.spawn_pty(&req).unwrap();
        pty.writer
            .write_all(b"stty size; echo rd-$((6*7)); exit\n")
            .unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        // The reader returns EOF (or an error) once the shell exited and its output is read.
        while let Ok(n @ 1..) = pty.reader.read(&mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("30 90"), "{text}");
        assert!(text.contains("rd-42"), "{text}");
    }
}
