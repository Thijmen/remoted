//! Changes, applied strictly in arrival order by one worker thread per channel: `mkdir`,
//! `rename`, `delete`, and uploads (`write`, FILE_DATA, `cancel`).
//!
//! Several uploads may be open at once and their FILE_DATA may interleave (the viewer pauses
//! one and continues another); each is kept by its `id`. An upload is written to a temporary
//! file next to its target (`.{name}.{token}.rdpart`) and moved into place after its END
//! frame. When the channel goes away, open uploads are kept for a while by token
//! ([`Interrupted`]), so a new channel can resume them.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use rd_proto::bin::FileAck;
use rd_proto::files::{Conflict, FilesAgentMsg, err};
use tracing::{debug, info, warn};

use super::Outbox;
use super::fsops::{self, Failure, Result};
use crate::platform::Platform;

/// What the reader hands the worker, in arrival order.
#[derive(Debug)]
pub(crate) enum Op {
    Mkdir {
        id: u32,
        path: String,
        exist_ok: bool,
    },
    Rename {
        id: u32,
        from: String,
        to: String,
    },
    Delete {
        id: u32,
        paths: Vec<String>,
        trash: bool,
    },
    Write {
        id: u32,
        path: String,
        size: u64,
        mtime_ms: Option<u64>,
        conflict: Conflict,
        token: String,
        resume: bool,
    },
    Data {
        seq: u32,
        id: u32,
        offset: u64,
        end: bool,
        data: Bytes,
    },
    Cancel {
        id: u32,
    },
}

/// An upload whose channel went away, kept so another channel can resume it.
struct Kept {
    part: PathBuf,
    target: PathBuf,
    generation: u64,
}

/// Interrupted uploads of all channels, by token. Each is deleted after `keep` unless a new
/// channel resumed it; the rest are deleted when the runtime goes away.
pub(crate) struct Interrupted {
    kept: Mutex<HashMap<String, Kept>>,
    /// Tokens of uploads open now, and the worker that has them.
    active: Mutex<HashMap<String, u64>>,
    generation: AtomicU64,
    keep: Duration,
    runtime: tokio::runtime::Handle,
}

impl Interrupted {
    pub(crate) fn new(keep: Duration, runtime: tokio::runtime::Handle) -> Arc<Self> {
        Arc::new(Interrupted {
            kept: Mutex::default(),
            active: Mutex::default(),
            generation: AtomicU64::new(0),
            keep,
            runtime,
        })
    }

    /// A number for a new worker.
    pub(crate) fn new_owner(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed)
    }

    fn activate(&self, token: &str, owner: u64) {
        self.active.lock().insert(token.to_owned(), owner);
    }

    /// The upload of `owner` with `token` is no longer open there. False when another worker
    /// has taken it over (a resume on a new channel raced the end of the old one).
    fn deactivate(&self, token: &str, owner: u64) -> bool {
        let mut active = self.active.lock();
        match active.get(token) {
            Some(&o) if o == owner => {
                active.remove(token);
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    fn keep(self: &Arc<Self>, token: String, part: PathBuf, target: PathBuf) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        debug!(part = %part.display(), "keeping an interrupted upload");
        if let Some(old) = self.kept.lock().insert(
            token.clone(),
            Kept {
                part,
                target,
                generation,
            },
        ) {
            let _ = fs::remove_file(old.part);
        }
        let this = Arc::downgrade(self);
        let keep = self.keep;
        self.runtime.spawn(async move {
            tokio::time::sleep(keep).await;
            if let Some(this) = this.upgrade() {
                this.expire(&token, generation);
            }
        });
    }

    fn expire(&self, token: &str, generation: u64) {
        let mut kept = self.kept.lock();
        if kept.get(token).is_some_and(|k| k.generation == generation) {
            let k = kept.remove(token).expect("just checked");
            info!(part = %k.part.display(), "an interrupted upload was not resumed; deleted");
            let _ = fs::remove_file(k.part);
        }
    }

    /// Another worker (an older channel whose socket has not noticed it is dead yet) has the
    /// upload with `token` open.
    fn open_elsewhere(&self, token: &str, owner: u64) -> bool {
        self.active.lock().get(token).is_some_and(|&o| o != owner)
    }

    /// The temporary file of an interrupted upload of `target` with `token`.
    fn take(&self, token: &str, target: &Path) -> Option<PathBuf> {
        let mut kept = self.kept.lock();
        if kept.get(token).is_some_and(|k| k.target == target) {
            kept.remove(token).map(|k| k.part)
        } else {
            None
        }
    }
}

impl Drop for Interrupted {
    fn drop(&mut self) {
        for (_, k) in self.kept.get_mut().drain() {
            let _ = fs::remove_file(k.part);
        }
    }
}

/// An upload in progress.
struct Upload {
    file: File,
    part: PathBuf,
    target: PathBuf,
    size: u64,
    mtime_ms: Option<u64>,
    conflict: Conflict,
    token: String,
    written: u64,
}

pub(crate) struct Worker {
    pub platform: Arc<dyn Platform>,
    pub out: Outbox,
    pub interrupted: Arc<Interrupted>,
    /// Bytes handed to the worker and not written yet (the reader adds, the worker subtracts).
    pub queued: Arc<AtomicU64>,
    owner: u64,
    uploads: HashMap<u32, Upload>,
}

impl Worker {
    pub(crate) fn new(
        platform: Arc<dyn Platform>,
        out: Outbox,
        interrupted: Arc<Interrupted>,
        queued: Arc<AtomicU64>,
    ) -> Self {
        let owner = interrupted.new_owner();
        Worker {
            platform,
            out,
            interrupted,
            queued,
            owner,
            uploads: HashMap::new(),
        }
    }

    /// Runs until the channel drops its sender; then keeps open uploads for resuming.
    pub(crate) fn run(mut self, ops: crossbeam_channel::Receiver<Op>) {
        for op in ops {
            self.apply(op);
        }
        for (_, up) in std::mem::take(&mut self.uploads) {
            drop(up.file);
            if self.interrupted.deactivate(&up.token, self.owner) {
                self.interrupted.keep(up.token, up.part, up.target);
            }
        }
    }

    /// Closes an upload that will not continue and deletes its temporary file.
    fn discard(&self, up: Upload) {
        drop(up.file);
        if self.interrupted.deactivate(&up.token, self.owner) {
            let _ = fs::remove_file(&up.part);
        }
    }

    fn reply(&self, id: u32, r: Result<()>) {
        match r {
            Ok(()) => self.out.control_json(&FilesAgentMsg::Ok { id }),
            Err(f) => self.fail(id, f),
        }
    }

    fn fail(&self, id: u32, f: Failure) {
        debug!(id, code = f.code, msg = f.msg, "file request failed");
        self.out.control_json(&FilesAgentMsg::Err {
            id,
            code: f.code.into(),
            msg: f.msg,
        });
    }

    pub(crate) fn apply(&mut self, op: Op) {
        match op {
            Op::Mkdir { id, path, exist_ok } => {
                let r = fsops::parse_new_path(&path).and_then(|p| {
                    fsops::mkdir(&p, exist_ok)?;
                    info!(path = %p.display(), "files: folder created");
                    Ok(())
                });
                self.reply(id, r);
            }
            Op::Rename { id, from, to } => {
                let r = fsops::parse_path(&from).and_then(|from| {
                    let to = fsops::parse_new_path(&to)?;
                    fsops::rename(&from, &to)?;
                    info!(from = %from.display(), to = %to.display(), "files: renamed");
                    Ok(())
                });
                self.reply(id, r);
            }
            Op::Delete { id, paths, trash } => {
                let r = paths
                    .iter()
                    .map(|p| fsops::parse_path(p))
                    .collect::<Result<Vec<_>>>()
                    .and_then(|paths| {
                        for p in &paths {
                            if p.parent().is_none() {
                                return Err(Failure::new(
                                    err::BAD_PATH,
                                    format!("{} is a root", p.display()),
                                ));
                            }
                        }
                        let trash = trash && self.platform.can_trash();
                        fsops::delete(self.platform.as_ref(), &paths, trash)?;
                        info!(paths = ?paths, trash, "files: deleted");
                        Ok(())
                    });
                self.reply(id, r);
            }
            Op::Write {
                id,
                path,
                size,
                mtime_ms,
                conflict,
                token,
                resume,
            } => {
                if let Some(old) = self.uploads.remove(&id) {
                    warn!(
                        id,
                        "a write reused the id of an open upload; dropping that one"
                    );
                    self.discard(old);
                }
                match self.open(&path, size, mtime_ms, conflict, token, resume) {
                    Ok(up) => {
                        if resume {
                            self.out.control_json(&FilesAgentMsg::Write {
                                id,
                                offset: up.written,
                            });
                        }
                        info!(
                            id,
                            path = %up.target.display(),
                            size,
                            offset = up.written,
                            "files: upload started"
                        );
                        self.interrupted.activate(&up.token, self.owner);
                        self.uploads.insert(id, up);
                    }
                    Err(f) => self.fail(id, f),
                }
            }
            Op::Data {
                seq,
                id,
                offset,
                end,
                data,
            } => {
                let len = data.len() as u64;
                self.on_data(id, offset, end, &data);
                self.queued.fetch_sub(len, Ordering::Relaxed);
                self.out.control_binary(FileAck { seq }.to_message());
            }
            Op::Cancel { id } => {
                if let Some(up) = self.uploads.remove(&id) {
                    info!(id, path = %up.target.display(), "files: upload cancelled");
                    self.discard(up);
                }
            }
        }
    }

    fn open(
        &self,
        path: &str,
        size: u64,
        mtime_ms: Option<u64>,
        conflict: Conflict,
        token: String,
        resume: bool,
    ) -> Result<Upload> {
        let target = fsops::parse_new_path(path)?;
        if token.is_empty()
            || token.len() > 64
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Failure::new(err::BAD_REQUEST, "invalid upload token"));
        }
        check_target(&target, conflict)?;
        let dir = target.parent().expect("a new path has a parent");
        let name = target
            .file_name()
            .expect("a new path has a name")
            .to_string_lossy();
        // A resume can arrive before the old channel's socket has died (the gateway notices a
        // dead viewer within 45 s): wait for that channel to park the upload rather than start a
        // second writer on the same temporary file.
        let deadline = std::time::Instant::now() + RESUME_TAKEOVER_WAIT;
        while self.interrupted.open_elsewhere(&token, self.owner) {
            if !resume || std::time::Instant::now() > deadline {
                return Err(Failure::new(
                    err::BAD_REQUEST,
                    "this upload is still open on another connection",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let kept = if resume {
            self.interrupted.take(&token, &target)
        } else {
            None
        };
        let part = kept
            .clone()
            .unwrap_or_else(|| dir.join(part_name(&name, &token)));
        let (file, written) = match kept {
            Some(_) => {
                let file = OpenOptions::new()
                    .append(true)
                    .open(&part)
                    .map_err(|e| Failure::io(&e, &part))?;
                let len = file.metadata().map_err(|e| Failure::io(&e, &part))?.len();
                (file, len)
            }
            None => (create_part(&part).map_err(|e| Failure::io(&e, dir))?, 0),
        };
        if written > size {
            drop(file);
            let _ = fs::remove_file(&part);
            return Err(Failure::new(
                err::BAD_REQUEST,
                "the kept upload is larger than the file",
            ));
        }
        Ok(Upload {
            file,
            part,
            target,
            size,
            mtime_ms,
            conflict,
            token,
            written,
        })
    }

    fn on_data(&mut self, id: u32, offset: u64, end: bool, data: &[u8]) {
        // Data of an upload that failed, was cancelled or never opened: dropped.
        let Some(up) = self.uploads.get_mut(&id) else {
            return;
        };
        let result = if offset != up.written {
            Err(Failure::new(
                err::BAD_REQUEST,
                format!("data at {offset}, expected {}", up.written),
            ))
        } else if up.written + data.len() as u64 > up.size {
            Err(Failure::new(err::BAD_REQUEST, "more data than the size"))
        } else {
            up.file
                .write_all(data)
                .map_err(|e| Failure::io(&e, &up.target))
                .map(|()| up.written += data.len() as u64)
        };
        if let Err(f) = result {
            let up = self.uploads.remove(&id).expect("present");
            self.discard(up);
            return self.fail(id, f);
        }
        if end {
            let up = self.uploads.remove(&id).expect("present");
            let part = up.part.clone();
            self.interrupted.deactivate(&up.token, self.owner);
            match finish(up) {
                Ok((path, size)) => {
                    info!(id, path = %path.display(), size, "files: upload done");
                    self.out.control_json(&FilesAgentMsg::Done {
                        id,
                        path: fsops::path_str(&path),
                        size,
                    });
                }
                Err(f) => {
                    let _ = fs::remove_file(&part);
                    self.fail(id, f);
                }
            }
        }
    }
}

/// How long a resume waits for an older channel to let go of its upload.
const RESUME_TAKEOVER_WAIT: Duration = Duration::from_secs(60);
/// Uploads at least this large are synced to disk before they are moved into place.
const SYNC_FROM_BYTES: u64 = 8 << 20;
/// Longest file name most file systems allow (bytes, or UTF-16 units on NTFS).
const NAME_MAX: usize = 255;

/// `.{name}.{token}.rdpart`, with `name` shortened (at a character boundary) so the whole stays
/// within NAME_MAX even for a target name that is itself near the limit.
fn part_name(name: &str, token: &str) -> String {
    let budget = NAME_MAX - ".".len() - ".".len() - token.len() - ".rdpart".len();
    let mut end = name.len().min(budget);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!(".{}.{token}.rdpart", &name[..end])
}

/// A new temporary file. Never one that exists: that would be another upload's. On Windows it
/// is hidden (the leading dot means nothing to Explorer).
fn create_part(part: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        options.attributes(FILE_ATTRIBUTE_HIDDEN);
    }
    match options.open(part) {
        // Left over from an agent that stopped mid-upload with the same token: not in use.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(part)?;
            options.open(part)
        }
        r => r,
    }
}

/// The conflict policy against what is at `target` now.
fn check_target(target: &Path, conflict: Conflict) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(target) else {
        return Ok(());
    };
    match conflict {
        Conflict::Fail => Err(Failure::new(
            err::EXISTS,
            format!("{} already exists", target.display()),
        )),
        Conflict::Replace if meta.is_dir() => Err(Failure::new(
            err::IS_DIR,
            format!("{} is a folder", target.display()),
        )),
        Conflict::Replace | Conflict::Rename => Ok(()),
    }
}

/// Moves a complete upload into place; returns its final path and size.
fn finish(up: Upload) -> Result<(PathBuf, u64)> {
    if up.written != up.size {
        return Err(Failure::new(
            err::BAD_REQUEST,
            format!("got {} of {} bytes", up.written, up.size),
        ));
    }
    if let Some(ms) = up.mtime_ms
        && let Err(e) = up.file.set_modified(fsops::system_time(ms))
    {
        debug!("cannot set the modification time: {e}");
    }
    // Large files are flushed to disk before they take the target's place. Doing that for every
    // small file of a folder upload would make it fsync-bound.
    if up.size >= SYNC_FROM_BYTES {
        up.file
            .sync_data()
            .map_err(|e| Failure::io(&e, &up.target))?;
    }
    drop(up.file);
    check_target(&up.target, up.conflict)?;
    let dir = up.target.parent().expect("a new path has a parent");
    let name = up
        .target
        .file_name()
        .expect("a new path has a name")
        .to_string_lossy()
        .into_owned();
    match up.conflict {
        Conflict::Replace => {
            fs::rename(&up.part, &up.target).map_err(|e| Failure::io(&e, &up.target))?;
            Ok((up.target, up.size))
        }
        Conflict::Fail | Conflict::Rename => {
            let mut target = up.target.clone();
            for _ in 0..1000 {
                if up.conflict == Conflict::Rename && fs::symlink_metadata(&target).is_ok() {
                    target = fsops::free_name(dir, &name);
                }
                match place_new(&up.part, &target) {
                    Ok(()) => return Ok((target, up.size)),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if up.conflict == Conflict::Fail {
                            return Err(Failure::io(&e, &target));
                        }
                    }
                    Err(e) => return Err(Failure::io(&e, &target)),
                }
            }
            Err(Failure::new(err::EXISTS, "no free name"))
        }
    }
}

/// Moves `part` to `target` unless something is there (AlreadyExists). A hard link makes that
/// atomic where the disk supports links; elsewhere it checks first.
fn place_new(part: &Path, target: &Path) -> io::Result<()> {
    match fs::hard_link(part, target) {
        Ok(()) => {
            let _ = fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if fs::symlink_metadata(target).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(part, target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_names_stay_within_name_max() {
        assert_eq!(part_name("a.txt", "0123"), ".a.txt.0123.rdpart");
        let token = "0123456789abcdef";
        let long = "x".repeat(250);
        assert_eq!(part_name(&long, token).len(), NAME_MAX);
        // Multi-byte characters are never cut in half.
        let wide = "é".repeat(200);
        let name = part_name(&wide, token);
        assert!(name.len() <= NAME_MAX);
        assert!(name.ends_with(".0123456789abcdef.rdpart"));
    }

    #[test]
    fn a_new_part_file_never_reuses_an_open_one() {
        let dir = std::env::temp_dir().join(format!("rd-part-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let part = dir.join(".a.tok.rdpart");
        let mut first = create_part(&part).unwrap();
        first.write_all(b"old").unwrap();
        // A leftover is replaced by a fresh, empty file.
        let second = create_part(&part).unwrap();
        assert_eq!(second.metadata().unwrap().len(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
