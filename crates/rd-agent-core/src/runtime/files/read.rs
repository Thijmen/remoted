//! Downloads: one at a time, in request order. A blocking producer reads the file (or builds
//! the zip) into a small channel; the sender cuts it into FILE_DATA frames as the
//! [`BulkWindow`] allows.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use rd_flow::BulkWindow;
use rd_proto::bin::{FileDataHeader, file_flags};
use rd_proto::files::{Expect, FilesAgentMsg, err};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use super::fsops::{self, Failure, Result};
use super::zip::ZipWriter;
use crate::clock::now_us;

/// Bytes a producer reads at a time.
const BLOCK: usize = 256 << 10;
/// Blocks a producer may read ahead of the window.
const AHEAD: usize = 4;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// A `read` request.
#[derive(Debug, Clone)]
pub(crate) struct ReadReq {
    pub id: u32,
    pub path: String,
    pub paths: Vec<String>,
    pub offset: u64,
    pub zip: bool,
    pub name: String,
    pub expect: Option<Expect>,
}

#[derive(Debug)]
pub(crate) enum DlCmd {
    Read(ReadReq),
    Cancel(u32),
}

/// The send window, shared with the reader (which applies FILE_ACKs).
pub(crate) struct Window {
    pub bulk: Mutex<BulkWindow>,
    pub acked: Notify,
}

impl Window {
    pub(crate) fn on_ack(&self, seq: u32) {
        if self.bulk.lock().on_ack(seq, now_us()) > 0 {
            self.acked.notify_one();
        }
    }
}

/// What a producer hands the sender, in order.
#[derive(Debug)]
enum Produced {
    /// The `read` answer that starts the transfer.
    Header(FilesAgentMsg),
    Data(Bytes),
    /// Other JSON in the transfer (zip `progress`, `skipped`).
    Json(FilesAgentMsg),
    Failed(Failure),
    End,
}

fn json(msg: &FilesAgentMsg) -> Message {
    Message::Text(
        serde_json::to_string(msg)
            .expect("file messages serialize")
            .into(),
    )
}

/// Runs the downloads of a channel until it closes.
pub(crate) async fn run(
    mut cmds: mpsc::UnboundedReceiver<DlCmd>,
    data: mpsc::Sender<Message>,
    window: Arc<Window>,
) {
    let mut queue: VecDeque<ReadReq> = VecDeque::new();
    loop {
        let req = match queue.pop_front() {
            Some(req) => req,
            None => match cmds.recv().await {
                Some(DlCmd::Read(req)) => req,
                Some(DlCmd::Cancel(_)) => continue,
                None => return,
            },
        };
        let mut transfer = Transfer {
            id: req.id,
            cmds: &mut cmds,
            queue: &mut queue,
            data: &data,
            window: &window,
        };
        match transfer.run(req).await {
            Ok(()) | Err(Stop::Cancelled) => {}
            Err(Stop::Closed) => return,
        }
    }
}

enum Stop {
    Cancelled,
    /// The channel is closing.
    Closed,
}

struct Transfer<'a> {
    id: u32,
    cmds: &'a mut mpsc::UnboundedReceiver<DlCmd>,
    queue: &'a mut VecDeque<ReadReq>,
    data: &'a mpsc::Sender<Message>,
    window: &'a Window,
}

impl Transfer<'_> {
    /// A command that arrived during the transfer: queue reads, apply cancels.
    fn on_cmd(&mut self, cmd: Option<DlCmd>) -> std::result::Result<(), Stop> {
        match cmd {
            None => Err(Stop::Closed),
            Some(DlCmd::Read(req)) => {
                self.queue.push_back(req);
                Ok(())
            }
            Some(DlCmd::Cancel(id)) if id == self.id => {
                info!(id, "files: download cancelled");
                Err(Stop::Cancelled)
            }
            Some(DlCmd::Cancel(id)) => {
                self.queue.retain(|r| r.id != id);
                Ok(())
            }
        }
    }

    async fn send(&self, msg: Message) -> std::result::Result<(), Stop> {
        self.data.send(msg).await.map_err(|_| Stop::Closed)
    }

    async fn run(&mut self, req: ReadReq) -> std::result::Result<(), Stop> {
        let (tx, mut rx) = mpsc::channel(AHEAD);
        let id = req.id;
        let mut offset = if req.zip { 0 } else { req.offset };
        tokio::task::spawn_blocking(move || produce(req, &tx));
        // Dropping `rx` (on return) stops the producer at its next send.
        loop {
            let item = tokio::select! {
                biased;
                cmd = self.cmds.recv() => {
                    self.on_cmd(cmd)?;
                    continue;
                }
                item = rx.recv() => item,
            };
            match item {
                Some(Produced::Header(msg) | Produced::Json(msg)) => self.send(json(&msg)).await?,
                Some(Produced::Data(mut bytes)) => {
                    while !bytes.is_empty() {
                        let chunk = self.wait_for_room(bytes.len()).await?;
                        let payload = bytes.split_to(chunk);
                        self.frame(0, offset, &payload).await?;
                        offset += chunk as u64;
                    }
                }
                Some(Produced::End) => {
                    self.wait_for_room(0).await?;
                    self.frame(file_flags::END, offset, &[]).await?;
                    debug!(id, bytes = offset, "download sent");
                    return Ok(());
                }
                Some(Produced::Failed(f)) => {
                    warn!(id, code = f.code, msg = f.msg, "files: download failed");
                    let msg = FilesAgentMsg::Err {
                        id,
                        code: f.code.into(),
                        msg: f.msg,
                    };
                    return self.send(json(&msg)).await;
                }
                None => {
                    let msg = FilesAgentMsg::Err {
                        id,
                        code: err::IO.into(),
                        msg: "the download stopped unexpectedly".into(),
                    };
                    return self.send(json(&msg)).await;
                }
            }
        }
    }

    /// Waits until the window takes a frame; returns how many of `want` bytes it may carry.
    async fn wait_for_room(&mut self, want: usize) -> std::result::Result<usize, Stop> {
        loop {
            let acked = self.window.acked.notified();
            tokio::pin!(acked);
            acked.as_mut().enable();
            {
                let bulk = self.window.bulk.lock();
                let chunk = (bulk.chunk_size() as usize).min(want);
                if bulk.can_send(chunk as u64) {
                    return Ok(chunk);
                }
            }
            tokio::select! {
                biased;
                cmd = self.cmds.recv() => self.on_cmd(cmd)?,
                () = &mut acked => {}
            }
        }
    }

    async fn frame(&self, flags: u8, offset: u64, payload: &[u8]) -> std::result::Result<(), Stop> {
        let seq = self
            .window
            .bulk
            .lock()
            .on_sent(payload.len() as u64, now_us());
        let header = FileDataHeader {
            flags,
            id: self.id,
            seq,
            offset,
        };
        self.send(Message::Binary(header.to_message(payload).into()))
            .await
    }
}

// ---------------------------------------------------------------------------
// producers (blocking threads)

/// Sends to the transfer; false once it is gone (cancelled, channel closed).
fn put(tx: &mpsc::Sender<Produced>, item: Produced) -> bool {
    tx.blocking_send(item).is_ok()
}

fn produce(req: ReadReq, tx: &mpsc::Sender<Produced>) {
    let result = if req.zip {
        produce_zip(&req, tx)
    } else {
        produce_file(&req, tx)
    };
    match result {
        Ok(true) => {
            put(tx, Produced::End);
        }
        Ok(false) => {}
        Err(f) => {
            put(tx, Produced::Failed(f));
        }
    }
}

/// One file from `offset`. Ok(false): the transfer went away.
fn produce_file(req: &ReadReq, tx: &mpsc::Sender<Produced>) -> Result<bool> {
    let path = fsops::parse_path(&req.path)?;
    let meta = fs::metadata(&path).map_err(|e| Failure::io(&e, &path))?;
    if meta.is_dir() {
        return Err(Failure::new(
            err::IS_DIR,
            format!("{} is a folder", path.display()),
        ));
    }
    if !meta.is_file() {
        return Err(Failure::new(
            err::BAD_REQUEST,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let mut file = File::open(&path).map_err(|e| Failure::io(&e, &path))?;
    let meta = file.metadata().map_err(|e| Failure::io(&e, &path))?;
    let size = meta.len();
    let mtime_ms = fsops::unix_ms(meta.modified());
    if let Some(expect) = req.expect
        && (expect.size != size || expect.mtime_ms != mtime_ms)
    {
        return Err(Failure::new(
            err::CHANGED,
            format!("{} changed since the download started", path.display()),
        ));
    }
    if req.offset > size {
        return Err(Failure::new(err::BAD_REQUEST, "offset beyond the end"));
    }
    info!(id = req.id, path = %path.display(), size, offset = req.offset, "files: download");
    let header = FilesAgentMsg::Read {
        id: req.id,
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: Some(size),
        mtime_ms: Some(mtime_ms),
        total: None,
        files: None,
    };
    if !put(tx, Produced::Header(header)) {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(req.offset))
        .map_err(|e| Failure::io(&e, &path))?;
    let mut left = size - req.offset;
    while left > 0 {
        let mut buf = BytesMut::zeroed(BLOCK.min(usize::try_from(left).unwrap_or(BLOCK)));
        let n = loop {
            match file.read(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Failure::io(&e, &path)),
            }
        };
        if n == 0 {
            return Err(Failure::new(
                err::CHANGED,
                format!("{} got shorter while it was read", path.display()),
            ));
        }
        buf.truncate(n);
        left -= n as u64;
        if !put(tx, Produced::Data(buf.freeze())) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Something to put in the archive.
#[derive(Debug)]
struct Item {
    /// Archive path ('/'-separated, no trailing slash).
    name: String,
    path: PathBuf,
    dir: bool,
    size: u64,
    mtime_s: i64,
}

fn mtime_s(meta: &fs::Metadata) -> i64 {
    (fsops::unix_ms(meta.modified()) / 1000) as i64
}

/// Collects the tree under `path`. Links to files are followed; links to folders only at the
/// top (`top`), so the walk cannot loop. Devices, sockets and pipes are left out.
fn walk(path: &Path, name: String, top: bool, items: &mut Vec<Item>, skipped: &mut Vec<String>) {
    let own = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return skipped.push(name),
    };
    let meta = if own.file_type().is_symlink() {
        match fs::metadata(path) {
            Ok(m) if m.is_file() || (top && m.is_dir()) => m,
            _ => return,
        }
    } else {
        own
    };
    if meta.is_file() {
        items.push(Item {
            name,
            path: path.to_path_buf(),
            dir: false,
            size: meta.len(),
            mtime_s: mtime_s(&meta),
        });
        return;
    }
    if !meta.is_dir() {
        return;
    }
    items.push(Item {
        name: name.clone(),
        path: path.to_path_buf(),
        dir: true,
        size: 0,
        mtime_s: mtime_s(&meta),
    });
    let Ok(dir) = fs::read_dir(path) else {
        return skipped.push(format!("{name}/"));
    };
    let mut children: Vec<(String, PathBuf)> = dir
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    children.sort();
    for (child, child_path) in children {
        walk(
            &child_path,
            format!("{name}/{child}"),
            false,
            items,
            skipped,
        );
    }
}

/// Turns zip output into `Data` blocks.
struct ChunkSink<'a> {
    tx: &'a mpsc::Sender<Produced>,
    buf: BytesMut,
}

impl ChunkSink<'_> {
    fn ship(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let block = self.buf.split().freeze();
        if put(self.tx, Produced::Data(block)) {
            Ok(())
        } else {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
}

impl Write for ChunkSink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= BLOCK {
            self.ship()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.ship()
    }
}

fn produce_zip(req: &ReadReq, tx: &mpsc::Sender<Produced>) -> Result<bool> {
    if req.paths.is_empty() {
        return Err(Failure::new(err::BAD_REQUEST, "nothing to zip"));
    }
    let paths = req
        .paths
        .iter()
        .map(|p| fsops::parse_path(p))
        .collect::<Result<Vec<_>>>()?;
    let parent = paths[0].parent();
    if parent.is_none() || paths.iter().any(|p| p.parent() != parent) {
        return Err(Failure::new(
            err::BAD_REQUEST,
            "the paths must be in one folder",
        ));
    }
    for p in &paths {
        fs::symlink_metadata(p).map_err(|e| Failure::io(&e, p))?;
    }
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for p in &paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        walk(p, name, true, &mut items, &mut skipped);
    }
    let total: u64 = items.iter().map(|i| i.size).sum();
    let files = items.iter().filter(|i| !i.dir).count() as u64;
    let name = if !req.name.is_empty() {
        req.name.clone()
    } else if paths.len() == 1 {
        format!("{}.zip", items.first().map_or("files", |i| i.name.as_str()))
    } else {
        parent
            .and_then(Path::file_name)
            .map_or("files.zip".to_owned(), |n| {
                format!("{}.zip", n.to_string_lossy())
            })
    };
    info!(id = req.id, paths = ?paths, total, files, "files: zip download");
    let header = FilesAgentMsg::Read {
        id: req.id,
        name,
        size: None,
        mtime_ms: None,
        total: Some(total),
        files: Some(files),
    };
    if !put(tx, Produced::Header(header)) {
        return Ok(false);
    }
    let gone = |e: &io::Error| e.kind() == io::ErrorKind::BrokenPipe;
    let mut zip = ZipWriter::new(ChunkSink {
        tx,
        buf: BytesMut::with_capacity(BLOCK + (64 << 10)),
    });
    let mut done = 0u64;
    let mut last_progress = Instant::now();
    for item in &items {
        if item.dir {
            match zip.add_dir(&item.name, item.mtime_s) {
                Ok(()) => continue,
                Err(e) if gone(&e) => return Ok(false),
                Err(e) => return Err(Failure::io(&e, &item.path)),
            }
        }
        let mut file = match File::open(&item.path) {
            Ok(f) => f,
            Err(e) => {
                debug!(path = %item.path.display(), "left out of the zip: {e}");
                skipped.push(item.name.clone());
                continue;
            }
        };
        let base = done;
        let mut progress = |n: u64| {
            if last_progress.elapsed() >= PROGRESS_EVERY {
                last_progress = Instant::now();
                let _ = tx.blocking_send(Produced::Json(FilesAgentMsg::Progress {
                    id: req.id,
                    done: base + n,
                }));
            }
        };
        match zip.add_file(
            &item.name,
            item.mtime_s,
            item.size,
            &mut file,
            &mut progress,
        ) {
            Ok(()) => done += item.size,
            Err(e) if gone(&e) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(Failure::new(
                    err::CHANGED,
                    format!("{} got shorter while it was zipped", item.path.display()),
                ));
            }
            Err(e) => return Err(Failure::io(&e, &item.path)),
        }
    }
    let sink = match zip.finish() {
        Ok(sink) => sink,
        Err(e) if gone(&e) => return Ok(false),
        Err(e) => return Err(Failure::new(err::IO, e.to_string())),
    };
    drop(sink);
    if !put(
        tx,
        Produced::Json(FilesAgentMsg::Progress { id: req.id, done }),
    ) {
        return Ok(false);
    }
    if !skipped.is_empty()
        && !put(
            tx,
            Produced::Json(FilesAgentMsg::Skipped {
                id: req.id,
                paths: skipped,
            }),
        )
    {
        return Ok(false);
    }
    Ok(true)
}
