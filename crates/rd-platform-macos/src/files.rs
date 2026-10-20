//! File browser support: places (home folders, the startup disk under its own name, mounted
//! volumes) and moving files to the Trash.

use std::fs;
use std::io;
use std::path::Path;

use objc2_foundation::{NSFileManager, NSString, NSURL};
use rd_agent_core::{Place, PlaceKind, home_places, place};

/// Home, Desktop, Documents, Downloads, `/` (named like the startup disk, e.g. "Macintosh HD")
/// and the other volumes in /Volumes.
pub fn places() -> Vec<Place> {
    let mut places = home_places();
    let mut root_name = "Macintosh HD".to_owned();
    let mut volumes = Vec::new();
    if let Ok(dir) = fs::read_dir("/Volumes") {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // The startup disk appears as a link to `/`.
            if fs::read_link(&path).is_ok_and(|t| t == Path::new("/")) {
                root_name = name;
                continue;
            }
            volumes.extend(place(PlaceKind::Volume, name, &path));
        }
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    places.extend(place(PlaceKind::Drive, root_name, Path::new("/")));
    places.extend(volumes);
    places
}

/// Cocoa error codes (FoundationErrors.h).
const NS_FILE_NO_SUCH_FILE: isize = 4;
const NS_FILE_READ_NO_PERMISSION: isize = 257;
const NS_FILE_WRITE_NO_PERMISSION: isize = 513;
const NS_FILE_WRITE_VOLUME_READ_ONLY: isize = 642;

/// Moves `path` to the Trash of its volume (NSFileManager, like Finder's Move to Trash).
pub fn trash(path: &Path) -> io::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let manager = NSFileManager::defaultManager();
    match manager.trashItemAtURL_resultingItemURL_error(&url, None) {
        Ok(()) => Ok(()),
        Err(e) => {
            let kind = match e.code() {
                NS_FILE_NO_SUCH_FILE => io::ErrorKind::NotFound,
                NS_FILE_READ_NO_PERMISSION | NS_FILE_WRITE_NO_PERMISSION => {
                    io::ErrorKind::PermissionDenied
                }
                NS_FILE_WRITE_VOLUME_READ_ONLY => io::ErrorKind::ReadOnlyFilesystem,
                _ => io::ErrorKind::Other,
            };
            Err(io::Error::new(kind, e.localizedDescription().to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_include_the_root() {
        let places = places();
        assert!(
            places
                .iter()
                .any(|p| p.kind == PlaceKind::Drive && p.path == "/"),
            "{places:?}"
        );
    }

    #[test]
    fn trash_reports_missing_files() {
        let err = trash(Path::new("/nonexistent/rd-trash-test")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    /// Puts a file in the Trash of whoever runs it: run by hand.
    #[test]
    #[ignore]
    fn trash_moves_a_file() {
        let dir = std::env::temp_dir().join(format!("rd-trash-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("trash-me.txt");
        fs::write(&file, b"x").unwrap();
        // Volumes without a Trash (some temp folders) refuse; then nothing changed.
        match trash(&file) {
            Ok(()) => assert!(!file.exists()),
            Err(e) => assert!(file.exists(), "{e}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
