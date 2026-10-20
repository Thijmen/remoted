//! Filesystem side of the file channel: path and name checks, entries, listing, error codes,
//! and the changes that are not uploads (mkdir, rename, delete).

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rd_proto::files::{Entry, EntryKind, MAX_LIST_ENTRIES, err};

use crate::platform::Platform;

/// A request that failed: an error code from [`rd_proto::files::err`] and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub msg: String,
}

impl Failure {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Failure {
        Failure {
            code,
            msg: msg.into(),
        }
    }

    /// An I/O error about `path`.
    pub fn io(e: &io::Error, path: &Path) -> Failure {
        let code = code_for(e);
        let mut msg = format!("{}: {e}", path.display());
        if code == err::DENIED && cfg!(target_os = "macos") {
            msg.push_str(
                " (macOS privacy: allow RD Agent in System Settings › Privacy & Security › Files \
                 and Folders, or give it Full Disk Access)",
            );
        }
        Failure { code, msg }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Failure>;

/// The error code for an I/O error.
pub(crate) fn code_for(e: &io::Error) -> &'static str {
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound => err::NOT_FOUND,
        K::AlreadyExists => err::EXISTS,
        K::PermissionDenied | K::ReadOnlyFilesystem => err::DENIED,
        K::NotADirectory => err::NOT_DIR,
        K::IsADirectory | K::DirectoryNotEmpty => err::IS_DIR,
        K::StorageFull | K::QuotaExceeded | K::FileTooLarge => err::NO_SPACE,
        K::InvalidFilename => err::BAD_NAME,
        K::Unsupported => err::UNSUPPORTED,
        _ => err::IO,
    }
}

// ---------------------------------------------------------------------------
// paths and names

/// An absolute path of this OS without `.` or `..` components, normalised (no trailing
/// separator, except at a root).
pub(crate) fn parse_path(s: &str) -> Result<PathBuf> {
    let bad = || Failure::new(err::BAD_PATH, format!("not an absolute path: {s:?}"));
    if s.is_empty() || s.contains('\0') {
        return Err(bad());
    }
    let path = Path::new(s);
    if !path.is_absolute() {
        return Err(bad());
    }
    // `components` drops inner `.` segments silently; look at the text too.
    let dots = s
        .split(|c| c == '/' || (cfg!(windows) && c == '\\'))
        .any(|seg| seg == "." || seg == "..");
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir | Component::ParentDir => {}
            c => {
                out.push(c.as_os_str());
                continue;
            }
        }
        return Err(dot_failure(s));
    }
    if dots {
        return Err(dot_failure(s));
    }
    Ok(out)
}

fn dot_failure(s: &str) -> Failure {
    Failure::new(
        err::BAD_PATH,
        format!("paths with . or .. are not accepted: {s:?}"),
    )
}

/// A path whose last component is a valid new name (see [`valid_name`]).
pub(crate) fn parse_new_path(s: &str) -> Result<PathBuf> {
    let path = parse_path(s)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Failure::new(err::BAD_PATH, format!("no file name in {s:?}")))?;
    if !valid_name(name, cfg!(windows)) {
        return Err(Failure::new(
            err::BAD_NAME,
            format!("{name:?} is not a valid name"),
        ));
    }
    Ok(path)
}

/// A name a new file or folder may get: not empty, `.` or `..`, no separator; on Windows also
/// none of `<>:"/\|?*`, no control characters, no trailing dot or space and no reserved device
/// name (CON, NUL, COM1, ...).
pub(crate) fn valid_name(name: &str, windows: bool) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return false;
    }
    if !windows {
        return true;
    }
    if name
        .chars()
        .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        || name.ends_with('.')
        || name.ends_with(' ')
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0');
    !reserved
}

/// `report.pdf` → `report (2).pdf`; `.bashrc` → `.bashrc (2)`; `Photos` → `Photos (2)`.
pub(crate) fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// The first `name (n).ext` (n ≥ 2) in `dir` that does not exist.
pub(crate) fn free_name(dir: &Path, name: &str) -> PathBuf {
    for n in 2..u32::MAX {
        let candidate = dir.join(numbered_name(name, n));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    dir.join(name)
}

/// The path as the protocol writes it (lossy for names that are not UTF-8).
pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// entries

pub(crate) fn unix_ms(t: io::Result<SystemTime>) -> u64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn system_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

#[cfg(windows)]
fn hidden_attr(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn hidden_attr(_meta: &fs::Metadata) -> bool {
    false
}

/// The entry for `path` named `name`, from its own metadata (`lstat`) and, for a link, its
/// target's.
pub(crate) fn entry_from(name: String, path: &Path, own: &fs::Metadata) -> Entry {
    let link = own.file_type().is_symlink();
    let target = if link {
        fs::metadata(path).ok()
    } else {
        Some(own.clone())
    };
    let hidden = name.starts_with('.') || hidden_attr(own);
    match target {
        Some(meta) => {
            let kind = if meta.is_dir() {
                EntryKind::Dir
            } else if meta.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            Entry {
                name,
                kind,
                size: if kind == EntryKind::File {
                    meta.len()
                } else {
                    0
                },
                mtime_ms: unix_ms(meta.modified()),
                hidden,
                link,
            }
        }
        None => Entry {
            name,
            kind: EntryKind::Other,
            size: 0,
            mtime_ms: unix_ms(own.modified()),
            hidden,
            link,
        },
    }
}

fn name_of(path: &Path) -> String {
    match path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        // A root: its own path ("/", "C:\").
        None => path_str(path),
    }
}

/// `stat`: `None` when nothing is there.
pub(crate) fn stat(path: &Path) -> Result<Option<Entry>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(entry_from(name_of(path), path, &meta))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Failure::io(&e, path)),
    }
}

/// A folder listing.
#[derive(Debug)]
pub(crate) struct Listing {
    pub parent: Option<PathBuf>,
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

pub(crate) fn list(path: &Path) -> Result<Listing> {
    let dir = fs::read_dir(path).map_err(|e| Failure::io(&e, path))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in dir {
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        let child = item.path();
        let entry = match fs::symlink_metadata(&child) {
            Ok(meta) => entry_from(name, &child, &meta),
            Err(_) => Entry {
                hidden: name.starts_with('.'),
                name,
                kind: EntryKind::Other,
                ..Entry::default()
            },
        };
        entries.push(entry);
    }
    Ok(Listing {
        parent: path.parent().map(Path::to_path_buf),
        entries,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// changes

pub(crate) fn mkdir(path: &Path, exist_ok: bool) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && exist_ok && path.is_dir() => Ok(()),
        Err(e) => Err(Failure::io(&e, path)),
    }
}

/// Whether `a` and `b` name the same file (a case-only rename on a case-insensitive disk).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    }
}

/// Renames without replacing anything.
pub(crate) fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::symlink_metadata(from).map_err(|e| Failure::io(&e, from))?;
    if fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
        return Err(Failure::new(
            err::EXISTS,
            format!("{} already exists", to.display()),
        ));
    }
    fs::rename(from, to).map_err(|e| Failure::io(&e, to))
}

/// Deletes `path` for good (a folder with everything in it; a link, not what it points to).
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        match fs::remove_file(path) {
            // A Windows symbolic link to a folder is removed like a folder.
            Err(e) if cfg!(windows) && meta.file_type().is_symlink() => {
                fs::remove_dir(path).map_err(|_| e)
            }
            r => r,
        }
    }
}

/// Deletes (or moves to the Trash) every path, stopping at the first failure; its message
/// lists what was not deleted. A path that does not exist counts as deleted.
pub(crate) fn delete(platform: &dyn Platform, paths: &[PathBuf], trash: bool) -> Result<()> {
    let trash = trash && platform.can_trash();
    for (i, path) in paths.iter().enumerate() {
        let result = if trash {
            fs::symlink_metadata(path).and_then(|_| platform.trash(path))
        } else {
            remove(path)
        };
        // Already gone is what was asked for (e.g. a request re-sent after a reconnect).
        if let Err(e) = result.or_else(|e| match e.kind() {
            io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        }) {
            let mut failure = Failure::io(&e, path);
            let left: Vec<String> = paths[i..].iter().map(|p| path_str(p)).collect();
            failure
                .msg
                .push_str(&format!("; not deleted: {}", left.join(", ")));
            return Err(failure);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a.txt", ".bashrc", "Ünïcødé ✓", "a b", "x..y"] {
            assert!(valid_name(ok, false), "{ok}");
            assert!(valid_name(ok, true), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(!valid_name(bad, false), "{bad:?}");
            assert!(!valid_name(bad, true), "{bad:?}");
        }
        for win_bad in [
            "a\\b", "a:b", "a?", "a*", "a<b", "a|b", "a\"b", "tab\t", "dot.", "space ", "CON",
            "nul.txt", "com1", "LPT9.log", "aux ",
        ] {
            assert!(valid_name(win_bad, false) || win_bad.contains('\0'));
            assert!(!valid_name(win_bad, true), "{win_bad:?}");
        }
        for win_ok in ["COM0", "COM10", "console", "nul2.txt", "LPT"] {
            assert!(valid_name(win_ok, true), "{win_ok:?}");
        }
    }

    #[test]
    fn numbered_names() {
        assert_eq!(numbered_name("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered_name(".bashrc", 3), ".bashrc (3)");
        assert_eq!(numbered_name("Photos", 2), "Photos (2)");
        assert_eq!(numbered_name("a.tar.gz", 2), "a.tar (2).gz");
    }

    #[cfg(unix)]
    #[test]
    fn paths() {
        assert_eq!(parse_path("/Users/me/").unwrap(), Path::new("/Users/me"));
        assert_eq!(parse_path("/").unwrap(), Path::new("/"));
        assert_eq!(parse_path("//a//b").unwrap(), Path::new("/a/b"));
        for bad in ["", "rel/x", "/a/../b", "/a/./b", "/a\0"] {
            assert_eq!(parse_path(bad).unwrap_err().code, err::BAD_PATH, "{bad:?}");
        }
        assert_eq!(parse_new_path("/").unwrap_err().code, err::BAD_PATH);
        assert!(parse_new_path("/tmp/x.txt").is_ok());
    }

    #[test]
    fn error_codes() {
        use io::ErrorKind as K;
        for (kind, code) in [
            (K::NotFound, err::NOT_FOUND),
            (K::AlreadyExists, err::EXISTS),
            (K::PermissionDenied, err::DENIED),
            (K::NotADirectory, err::NOT_DIR),
            (K::IsADirectory, err::IS_DIR),
            (K::StorageFull, err::NO_SPACE),
            (K::Unsupported, err::UNSUPPORTED),
            (K::Interrupted, err::IO),
        ] {
            assert_eq!(code_for(&io::Error::from(kind)), code, "{kind:?}");
        }
        let denied = Failure::io(&io::Error::from(K::PermissionDenied), Path::new("/x"));
        assert!(denied.msg.starts_with("/x: "), "{}", denied.msg);
    }

    #[test]
    fn listing_and_stat() {
        let dir = tempdir("list");
        fs::write(dir.join("a.txt"), b"hello").unwrap();
        fs::write(dir.join(".hidden"), b"").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("sub"), dir.join("to-sub")).unwrap();
            std::os::unix::fs::symlink(dir.join("gone"), dir.join("broken")).unwrap();
        }
        let mut l = list(&dir).unwrap();
        l.entries.sort_by(|a, b| a.name.cmp(&b.name));
        let get = |n: &str| l.entries.iter().find(|e| e.name == n).unwrap().clone();
        assert_eq!(get("a.txt").kind, EntryKind::File);
        assert_eq!(get("a.txt").size, 5);
        assert!(get("a.txt").mtime_ms > 0);
        assert!(get(".hidden").hidden);
        assert_eq!(get("sub").kind, EntryKind::Dir);
        #[cfg(unix)]
        {
            assert_eq!(get("to-sub").kind, EntryKind::Dir);
            assert!(get("to-sub").link);
            assert_eq!(get("broken").kind, EntryKind::Other);
        }
        assert_eq!(l.parent.as_deref(), dir.parent());
        assert_eq!(stat(&dir.join("nope")).unwrap(), None);
        assert_eq!(stat(&dir.join("a.txt")).unwrap().unwrap().name, "a.txt");
        assert_eq!(list(&dir.join("a.txt")).unwrap_err().code, err::NOT_DIR);
        assert_eq!(list(&dir.join("nope")).unwrap_err().code, err::NOT_FOUND);

        mkdir(&dir.join("new"), false).unwrap();
        assert_eq!(
            mkdir(&dir.join("new"), false).unwrap_err().code,
            err::EXISTS
        );
        mkdir(&dir.join("new"), true).unwrap();
        assert_eq!(
            mkdir(&dir.join("a.txt"), true).unwrap_err().code,
            err::EXISTS
        );
        assert_eq!(
            rename(&dir.join("a.txt"), &dir.join("sub"))
                .unwrap_err()
                .code,
            err::EXISTS
        );
        rename(&dir.join("a.txt"), &dir.join("b.txt")).unwrap();
        assert_eq!(
            rename(&dir.join("a.txt"), &dir.join("c.txt"))
                .unwrap_err()
                .code,
            err::NOT_FOUND
        );
        assert_eq!(free_name(&dir, "b.txt"), dir.join("b (2).txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    pub(crate) fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rd-files-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        // macOS: /var/folders/... is a link to /private/var/...; keep paths canonical.
        dir.canonicalize().unwrap()
    }
}
