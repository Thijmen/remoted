//! File browser support: places (the user's known folders where they really are, e.g. a
//! Desktop moved to OneDrive, and the drive letters) and moving files to the Recycle Bin.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rd_agent_core::{Place, PlaceKind, place};
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOLDERID_Desktop,
    FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Profile, KF_FLAG_DEFAULT, SHFILEOPSTRUCTW,
    SHFileOperationW, SHGetKnownFolderPath,
};
use windows::core::{GUID, PCWSTR};

// GetDriveTypeW results (WinBase.h).
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Where a known folder is (`None` if it has none).
fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the returned buffer is a NUL-terminated string allocated by the shell; it is
    // copied, then freed with CoTaskMemFree as documented.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as *const _));
        path
    }
}

/// "Label (C:)" for local drives with a label, else "C:". Network drives are not asked for
/// their label (that can hang while the server is unreachable).
fn drive_name(letter: char, root: &[u16], kind: u32) -> String {
    if matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE) {
        let mut label = [0u16; 261];
        // SAFETY: `root` is NUL-terminated; the buffer outlives the call.
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                None,
            )
        }
        .is_ok();
        if ok {
            let len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
            let label = String::from_utf16_lossy(&label[..len]);
            if !label.is_empty() {
                return format!("{label} ({letter}:)");
            }
        }
    }
    format!("{letter}:")
}

/// Home, Desktop, Documents, Downloads (known folders) and the drives.
pub fn places() -> Vec<Place> {
    let mut places = Vec::new();
    let home = known_folder(&FOLDERID_Profile).or_else(rd_agent_core::home_dir);
    if let Some(home) = &home {
        let name = home
            .file_name()
            .map_or("Home".to_owned(), |n| n.to_string_lossy().into_owned());
        places.extend(place(PlaceKind::Home, name, home));
    }
    for (kind, name, id, fallback) in [
        (PlaceKind::Desktop, "Desktop", &FOLDERID_Desktop, "Desktop"),
        (
            PlaceKind::Documents,
            "Documents",
            &FOLDERID_Documents,
            "Documents",
        ),
        (
            PlaceKind::Downloads,
            "Downloads",
            &FOLDERID_Downloads,
            "Downloads",
        ),
    ] {
        let path = known_folder(id).or_else(|| home.as_ref().map(|h| h.join(fallback)));
        if let Some(path) = path {
            places.extend(place(kind, name, &path));
        }
    }
    // SAFETY: no arguments.
    let drives = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if drives & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i as u8);
        let root = format!("{letter}:\\");
        let root_w = wide(std::ffi::OsStr::new(&root));
        // SAFETY: `root_w` is NUL-terminated.
        let kind = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
        if !matches!(
            kind,
            DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_REMOTE | DRIVE_CDROM
        ) {
            continue;
        }
        places.push(Place {
            kind: PlaceKind::Drive,
            name: drive_name(letter, &root_w, kind),
            path: root,
        });
    }
    places
}

/// Only local fixed drives have a Recycle Bin. On network shares and removable drives
/// SHFileOperation would delete for good while reporting success, so those are refused.
fn has_recycle_bin(path: &Path) -> bool {
    let s = path.as_os_str().to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let b = s.as_bytes();
    // UNC paths (\\server\share) have no drive letter.
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return false;
    }
    let root = wide(std::ffi::OsStr::new(&format!("{}:\\", b[0] as char)));
    // SAFETY: `root` is a NUL-terminated wide string that lives across the call.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

/// Moves `path` to the Recycle Bin (SHFileOperation with FOF_ALLOWUNDO). Drives without one
/// (network shares, removable drives) answer Unsupported, so the viewer can offer a permanent
/// delete instead. (An item too large for the Recycle Bin is still deleted for good, as
/// Explorer does after asking; there is no one at the remote screen to ask.)
pub fn trash(path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(path).is_err() {
        return Err(io::ErrorKind::NotFound.into());
    }
    if !has_recycle_bin(path) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this drive has no Recycle Bin",
        ));
    }
    // pFrom is a list: each path NUL-terminated, the list ends with another NUL.
    let mut from = wide(path.as_os_str());
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    // SAFETY: `op` and the string it points to live across the call.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "moving to the Recycle Bin failed (error {rc:#x})"
        )));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(io::Error::other("moving to the Recycle Bin was cancelled"));
    }
    Ok(())
}
