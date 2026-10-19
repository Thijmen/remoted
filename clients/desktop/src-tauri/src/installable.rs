//! Can this copy of rd Desktop replace itself? Installed copies can: Windows through the NSIS
//! installer (which wrote `uninstall.exe` next to the exe), macOS by swapping its `.app` bundle.
//! Others only offer to download the new version: the portable exe, an app on the read-only
//! disk image, one that Gatekeeper translocated, one in a folder the user cannot write, and
//! development builds (a bare binary in `target/`, which the updater would overwrite with an app).

use std::path::{Path, PathBuf};

/// Why this copy cannot install an update. Each platform has only some of these.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Blocker {
    /// Windows: no `uninstall.exe` next to the exe.
    Portable,
    /// Not an installed app: a bare binary (development build).
    NotInstalled,
    /// macOS: run from the (read-only) disk image.
    DiskImage,
    /// macOS: Gatekeeper runs it from a read-only copy (App Translocation).
    Translocated,
    /// macOS: the user cannot write the bundle or the folder it is in.
    ReadOnly(PathBuf),
    /// Neither Windows nor macOS.
    Unsupported,
}

impl Blocker {
    /// Completes "This copy cannot update itself: …" in the update window.
    pub fn describe(&self) -> String {
        match self {
            Self::Portable => {
                "it is the portable exe. The installer (…-setup.exe) keeps itself up to date."
                    .into()
            }
            Self::NotInstalled => "it is not an installed app (a development build).".into(),
            Self::DiskImage => {
                "it runs from the disk image. Drag rd Desktop to Applications and start it from there."
                    .into()
            }
            Self::Translocated => "macOS runs it from a temporary read-only copy. Move rd Desktop \
                 to Applications with the Finder and start it from there."
                .into(),
            Self::ReadOnly(dir) => format!("you cannot write to {}.", dir.display()),
            Self::Unsupported => "updates are not supported on this system.".into(),
        }
    }
}

/// Why the running copy cannot update itself, or `None` if it can.
pub fn blocker() -> Option<Blocker> {
    match tauri::utils::platform::current_exe() {
        Ok(exe) => blocker_for(&exe),
        Err(e) => {
            log::warn!("updates: cannot tell where the app is installed: {e}");
            Some(Blocker::NotInstalled)
        }
    }
}

#[cfg(windows)]
fn blocker_for(exe: &Path) -> Option<Blocker> {
    (!nsis_installed(exe)).then_some(Blocker::Portable)
}

#[cfg(target_os = "macos")]
fn blocker_for(exe: &Path) -> Option<Blocker> {
    /// `EROFS`: a read-only file system (a disk image, App Translocation).
    const EROFS: i32 = 30;
    let Some(bundle) = app_bundle(exe) else {
        return Some(Blocker::NotInstalled);
    };
    if is_translocated(&bundle) {
        return Some(Blocker::Translocated);
    }
    // The updater moves the bundle aside and the new one in: that needs both.
    let parent = bundle.parent()?;
    for dir in [parent, bundle.as_path()] {
        if let Err(e) = writable(dir) {
            log::info!("updates: cannot write to {}: {e}", dir.display());
            return Some(
                if e.raw_os_error() == Some(EROFS) && bundle.starts_with("/Volumes") {
                    Blocker::DiskImage
                } else {
                    Blocker::ReadOnly(dir.to_path_buf())
                },
            );
        }
    }
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
fn blocker_for(_exe: &Path) -> Option<Blocker> {
    Some(Blocker::Unsupported)
}

/// Windows: did the NSIS installer put this exe there? Its uninstaller sits next to it; the
/// portable exe has none.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn nsis_installed(exe: &Path) -> bool {
    exe.with_file_name("uninstall.exe").is_file()
}

/// macOS: the `.app` bundle of `…/X.app/Contents/MacOS/<binary>`, or `None` for a bare binary.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// macOS: Gatekeeper's App Translocation runs a quarantined app that was not moved by the
/// Finder from a random read-only path (`…/AppTranslocation/<uuid>/d/X.app`).
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
}

/// May this process write to `path`? `access(2)`: the real permissions, read-only volumes
/// included, unlike the mode bits.
#[cfg(target_os = "macos")]
fn writable(path: &Path) -> std::io::Result<()> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn access(path: *const c_char, mode: c_int) -> c_int;
    }
    const W_OK: c_int = 2;
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: a valid NUL-terminated path.
    if unsafe { access(path.as_ptr(), W_OK) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_installed_or_portable() {
        let dir = std::env::temp_dir().join(format!("rd-desktop-test-nsis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rd-desktop.exe");
        std::fs::write(&exe, b"").unwrap();
        assert!(!nsis_installed(&exe), "portable: no uninstaller");
        std::fs::write(dir.join("uninstall.exe"), b"").unwrap();
        assert!(nsis_installed(&exe));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mac_bundles() {
        let exe = Path::new("/Applications/rd Desktop.app/Contents/MacOS/rd-desktop");
        assert_eq!(
            app_bundle(exe).as_deref(),
            Some(Path::new("/Applications/rd Desktop.app"))
        );
        assert_eq!(app_bundle(Path::new("/x/target/debug/rd-desktop")), None);
        assert_eq!(
            app_bundle(Path::new("/x/rd Desktop/Contents/MacOS/rd-desktop")),
            None
        );
        assert_eq!(app_bundle(Path::new("rd-desktop")), None);

        assert!(is_translocated(Path::new(
            "/private/var/folders/ab/xyz/T/AppTranslocation/0F2C-11/d/rd Desktop.app"
        )));
        assert!(!is_translocated(Path::new("/Applications/rd Desktop.app")));
        assert!(!is_translocated(Path::new(
            "/Users/me/AppTranslocationNotes/rd Desktop.app"
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_install_locations() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("rd-desktop-test-bundle-{}", std::process::id()));
        let bundle = root.join("rd Desktop.app");
        let exe = bundle.join("Contents/MacOS/rd-desktop");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(blocker_for(&exe), None);
        assert_eq!(
            blocker_for(&root.join("rd-desktop")),
            Some(Blocker::NotInstalled)
        );
        // A folder the user cannot write (root can write anywhere).
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: no preconditions.
        if unsafe { geteuid() } != 0 {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
            assert_eq!(blocker_for(&exe), Some(Blocker::ReadOnly(root.clone())));
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&root).unwrap();
        // The system volume is read-only, but not a disk image.
        assert!(matches!(
            blocker_for(Path::new(
                "/System/Applications/Calculator.app/Contents/MacOS/Calculator"
            )),
            Some(Blocker::ReadOnly(_))
        ));
    }
}
