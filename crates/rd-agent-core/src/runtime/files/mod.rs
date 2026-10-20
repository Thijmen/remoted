//! File channels (docs/files.md): browse, download and upload files on their own sockets
//! (`/ws/agent/files/{channel}`), next to and independent of the screen session.
//!
//! ```text
//! Channels      at most 4 at a time; stopped when the runtime shuts down
//!   └─ channel  one task per socket: reads and dispatches, owns the timers
//!        ├─ writer      sole owner of the sink; control (replies, FILE_ACK, PONG, state)
//!        │              jumps ahead of the data queue (download headers and FILE_DATA)
//!        ├─ list/stat   answered on blocking threads, concurrently
//!        ├─ worker      (write.rs) a thread applying changes and uploads in arrival order
//!        └─ downloads   (read.rs) one at a time through a BulkWindow
//! ```

mod fsops;
mod read;
mod write;
mod zip;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use rd_flow::{BulkConfig, BulkWindow, TARGET_FULL_US, TARGET_GENTLE_US};
use rd_proto::bin::{self, BinMsg};
use rd_proto::close;
use rd_proto::control::AgentState;
use rd_proto::files::{FILES_PROTO_VERSION, FilesAgentMsg, FilesViewerMsg, MAX_MESSAGE, caps, err};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{Instrument as _, debug, info, warn};

use super::Ctx;
use super::net::WsStream;
use super::task::TaskGuard;
use crate::clock::now_us;
use fsops::{Failure, path_str};
use read::{DlCmd, ReadReq, Window};
use write::{Op, Worker};

pub(crate) use write::Interrupted;

/// File channels at a time. The gateway allows 4 per machine and frees a slot as soon as the
/// viewer's side ends, while this side may still be closing; the margin keeps such a channel
/// from blocking a new one. This cap only guards against a gateway that does not limit.
pub(crate) const MAX_CHANNELS: usize = 8;
/// Data queue between the downloads and the writer; the window already bounds it.
const DATA_QUEUE: usize = 4;
/// Upload bytes waiting for the worker beyond which the viewer ignores the window: the
/// channel is closed.
const MAX_QUEUED: u64 = 64 << 20;
/// How long shutdown waits for a channel to close politely.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// The file channels of the runtime.
pub(crate) struct Channels {
    ctx: Arc<Ctx>,
    running: Vec<Running>,
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: TaskGuard<()>,
}

impl Channels {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Channels {
            ctx,
            running: Vec::new(),
        }
    }

    /// The gateway asked for channel `id` (already validated).
    pub(crate) fn start(&mut self, id: String, user: String) {
        self.running.retain(|r| !r.task.is_done());
        if self.running.len() >= MAX_CHANNELS {
            warn!(channel = id, "too many file channels; request ignored");
            return;
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        let span = tracing::info_span!("files", id = %&id[..id.len().min(8)]);
        let task = TaskGuard::spawn(run(self.ctx.clone(), id, user, stop_rx).instrument(span));
        self.running.push(Running {
            stop: Some(stop_tx),
            task,
        });
    }

    /// Closes every channel (politely, within a grace period).
    pub(crate) async fn shutdown(&mut self) {
        for r in &mut self.running {
            if let Some(stop) = r.stop.take() {
                let _ = stop.send(());
            }
        }
        for mut r in self.running.drain(..) {
            if timeout(SHUTDOWN_GRACE, &mut r.task).await.is_err() {
                debug!("a file channel did not close in time; aborted");
            }
        }
    }
}

/// Non-blocking sender for the writer's control queue (usable from blocking threads).
#[derive(Clone)]
pub(crate) struct Outbox(mpsc::UnboundedSender<Message>);

impl Outbox {
    pub(crate) fn control_json(&self, msg: &FilesAgentMsg) {
        match serde_json::to_string(msg) {
            Ok(text) => {
                let _ = self.0.send(Message::Text(text.into()));
            }
            Err(e) => warn!("serializing a file message: {e}"),
        }
    }

    pub(crate) fn control_binary(&self, data: Vec<u8>) {
        let _ = self.0.send(Message::Binary(data.into()));
    }

    fn err(&self, id: u32, f: Failure) {
        self.control_json(&FilesAgentMsg::Err {
            id,
            code: f.code.into(),
            msg: f.msg,
        });
    }
}

/// How a channel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Closed,
    Failed,
    Idle,
    HelloTimeout,
    Protocol,
    Shutdown,
}

impl End {
    fn close_code(self) -> u16 {
        match self {
            End::Protocol => close::PROTOCOL,
            End::Shutdown => 1001,
            _ => 1000,
        }
    }
}

async fn run(ctx: Arc<Ctx>, id: String, user: String, mut stop: oneshot::Receiver<()>) {
    let url = ctx.gateway.files_url(&id);
    let dial = timeout(
        ctx.timings.dial_timeout,
        ctx.gateway.dial(&url, MAX_MESSAGE),
    );
    let ws = tokio::select! {
        _ = &mut stop => return,
        r = dial => match r {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                warn!("dialing the file channel failed: {e:#}");
                return;
            }
            Err(_) => {
                warn!("dialing the file channel timed out");
                return;
            }
        },
    };
    info!(user, "files: channel open");
    let end = Channel::start(ctx, ws).run(&mut stop).await;
    info!(user, end = ?end, "files: channel closed");
}

struct Channel {
    ctx: Arc<Ctx>,
    out: Outbox,
    /// Kept so the writer runs until we close.
    _data: mpsc::Sender<Message>,
    writer: TaskGuard<()>,
    stream: SplitStream<WsStream>,
    ops: Option<crossbeam_channel::Sender<Op>>,
    worker: Option<std::thread::JoinHandle<()>>,
    queued: Arc<AtomicU64>,
    downloads: mpsc::UnboundedSender<DlCmd>,
    _downloader: TaskGuard<()>,
    window: Arc<Window>,
    hello: bool,
}

impl Channel {
    fn start(ctx: Arc<Ctx>, ws: WsStream) -> Channel {
        let (sink, stream) = ws.split();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (data_tx, data_rx) = mpsc::channel(DATA_QUEUE);
        let writer = TaskGuard::spawn(write_loop(sink, control_rx, data_rx));
        let out = Outbox(control_tx);
        let target = target_for(&ctx.state.borrow());
        let window = Arc::new(Window {
            bulk: Mutex::new(BulkWindow::new(BulkConfig::default(), target)),
            acked: Notify::new(),
        });
        let (dl_tx, dl_rx) = mpsc::unbounded_channel();
        let downloader = TaskGuard::spawn(read::run(dl_rx, data_tx.clone(), window.clone()));
        let queued = Arc::new(AtomicU64::new(0));
        let (ops_tx, ops_rx) = crossbeam_channel::unbounded();
        let worker = Worker::new(
            ctx.platform.clone(),
            out.clone(),
            ctx.interrupted.clone(),
            queued.clone(),
        );
        let worker = std::thread::Builder::new()
            .name("rd-files".into())
            .spawn(move || worker.run(ops_rx))
            .ok();
        Channel {
            ctx,
            out,
            _data: data_tx,
            writer,
            stream,
            ops: Some(ops_tx),
            worker,
            queued,
            downloads: dl_tx,
            _downloader: downloader,
            window,
            hello: false,
        }
    }

    async fn run(mut self, stop: &mut oneshot::Receiver<()>) -> End {
        let mut state = self.ctx.state.subscribe();
        state.mark_unchanged();
        let hello_deadline = tokio::time::sleep(self.ctx.timings.hello_timeout);
        tokio::pin!(hello_deadline);
        let idle = self.ctx.timings.idle_timeout;
        let end = loop {
            let awaiting_hello = !self.hello;
            tokio::select! {
                biased;
                _ = &mut *stop => break End::Shutdown,
                _ = &mut self.writer => break End::Failed,
                msg = timeout(idle, self.stream.next()) => {
                    let msg = match msg {
                        Err(_) => break End::Idle,
                        Ok(None) => break End::Closed,
                        Ok(Some(Err(e))) => {
                            debug!("file channel failed: {e}");
                            break End::Failed;
                        }
                        Ok(Some(Ok(msg))) => msg,
                    };
                    let step = match msg {
                        Message::Text(text) => self.on_text(text.as_str()),
                        Message::Binary(data) => self.on_binary(data),
                        Message::Close(_) => Err(End::Closed),
                        _ => Ok(()),
                    };
                    if let Err(end) = step {
                        break end;
                    }
                }
                Ok(()) = state.changed() => {
                    let s = *state.borrow_and_update();
                    self.window.bulk.lock().set_target(target_for(&s));
                    if self.hello {
                        self.out.control_json(&FilesAgentMsg::State(s));
                    }
                }
                () = &mut hello_deadline, if awaiting_hello => {
                    warn!("the viewer did not say hello on the file channel");
                    break End::HelloTimeout;
                }
            }
        };
        self.close(end).await;
        end
    }

    fn on_binary(&mut self, data: Bytes) -> Result<(), End> {
        match bin::parse(&data) {
            Ok(BinMsg::FileData(h, _)) => {
                let payload = data.slice(bin::FILE_DATA_HEADER_LEN..);
                let len = payload.len() as u64;
                if self.queued.fetch_add(len, Ordering::Relaxed) + len > MAX_QUEUED {
                    warn!("the viewer sends more upload data than it may; closing");
                    return Err(End::Protocol);
                }
                self.op(Op::Data {
                    seq: h.seq,
                    id: h.id,
                    offset: h.offset,
                    end: h.is_end(),
                    data: payload,
                });
            }
            Ok(BinMsg::FileAck(ack)) => self.window.on_ack(ack.seq),
            Ok(BinMsg::Ping(ping)) => self.out.control_binary(ping.pong(now_us()).to_message()),
            Ok(_) => {}
            Err(e) => debug!("malformed binary message on a file channel: {e}"),
        }
        Ok(())
    }

    fn op(&self, op: Op) {
        if let Some(ops) = &self.ops {
            let _ = ops.send(op);
        }
    }

    fn on_text(&mut self, text: &str) -> Result<(), End> {
        let msg = match serde_json::from_str::<FilesViewerMsg>(text) {
            Ok(msg) => msg,
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|v| v["id"].as_u64())
                    .and_then(|id| u32::try_from(id).ok())
                    .unwrap_or(0);
                self.out.err(
                    id,
                    Failure::new(err::BAD_REQUEST, format!("invalid message: {e}")),
                );
                return Ok(());
            }
        };
        if let FilesViewerMsg::Hello { proto } = msg {
            return self.on_hello(proto);
        }
        if matches!(msg, FilesViewerMsg::Unknown) {
            debug!("unknown file message ignored");
            return Ok(());
        }
        if !self.hello {
            self.out.err(
                request_id(&msg),
                Failure::new(err::BAD_REQUEST, "send hello first"),
            );
            return Ok(());
        }
        match msg {
            FilesViewerMsg::Hello { .. } | FilesViewerMsg::Unknown => {}
            FilesViewerMsg::List { id, path } => self.query(id, path, true),
            FilesViewerMsg::Stat { id, path } => self.query(id, path, false),
            FilesViewerMsg::Mkdir { id, path, exist_ok } => {
                self.op(Op::Mkdir { id, path, exist_ok })
            }
            FilesViewerMsg::Rename { id, from, to } => self.op(Op::Rename { id, from, to }),
            FilesViewerMsg::Delete { id, paths, trash } => self.op(Op::Delete { id, paths, trash }),
            FilesViewerMsg::Write {
                id,
                path,
                size,
                mtime_ms,
                conflict,
                token,
                resume,
            } => self.op(Op::Write {
                id,
                path,
                size,
                mtime_ms,
                conflict,
                token,
                resume,
            }),
            FilesViewerMsg::Read {
                id,
                path,
                paths,
                offset,
                zip,
                name,
                expect,
            } => {
                let _ = self.downloads.send(DlCmd::Read(ReadReq {
                    id,
                    path,
                    paths,
                    offset,
                    zip,
                    name,
                    expect,
                }));
            }
            FilesViewerMsg::Cancel { id } => {
                let _ = self.downloads.send(DlCmd::Cancel(id));
                self.op(Op::Cancel { id });
            }
        }
        Ok(())
    }

    fn on_hello(&mut self, proto: u16) -> Result<(), End> {
        if self.hello {
            debug!("duplicate hello ignored");
            return Ok(());
        }
        if proto < FILES_PROTO_VERSION {
            self.out.control_json(&FilesAgentMsg::Err {
                id: 0,
                code: err::PROTO_MISMATCH.into(),
                msg: format!(
                    "the viewer speaks file protocol {proto}, the agent {FILES_PROTO_VERSION}"
                ),
            });
            return Err(End::Protocol);
        }
        self.hello = true;
        let platform = &self.ctx.platform;
        let places = platform.file_places();
        let home = places
            .iter()
            .find(|p| p.kind == rd_proto::files::PlaceKind::Home)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let mut caps_list = vec![caps::ZIP.to_owned(), caps::RESUME.to_owned()];
        if platform.can_trash() {
            caps_list.insert(0, caps::TRASH.to_owned());
        }
        self.out.control_json(&FilesAgentMsg::Hello {
            proto: FILES_PROTO_VERSION,
            os: platform.os().into(),
            sep: std::path::MAIN_SEPARATOR_STR.into(),
            home,
            places,
            caps: caps_list,
            state: *self.ctx.state.borrow(),
        });
        Ok(())
    }

    /// `list` / `stat`, answered on a blocking thread.
    fn query(&self, id: u32, path: String, list: bool) {
        let out = self.out.clone();
        tokio::task::spawn_blocking(move || {
            let reply = fsops::parse_path(&path).and_then(|p| {
                if list {
                    let l = fsops::list(&p)?;
                    Ok(FilesAgentMsg::List {
                        id,
                        path: path_str(&p),
                        parent: l.parent.as_deref().map(path_str),
                        entries: l.entries,
                        truncated: l.truncated,
                    })
                } else {
                    Ok(FilesAgentMsg::Stat {
                        id,
                        entry: fsops::stat(&p)?,
                    })
                }
            });
            match reply {
                Ok(msg) => out.control_json(&msg),
                Err(f) => out.err(id, f),
            }
        });
    }

    async fn close(mut self, end: End) {
        // The worker finishes what it has (and keeps open uploads for resuming).
        drop(self.ops.take());
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
        let frame = CloseFrame {
            code: CloseCode::from(end.close_code()),
            reason: match end {
                End::Protocol => "protocol error",
                End::Shutdown => "agent shutting down",
                _ => "closed",
            }
            .into(),
        };
        let _ = self.out.0.send(Message::Close(Some(frame)));
        let Channel {
            out,
            _data,
            mut writer,
            _downloader,
            ..
        } = self;
        drop((out, _data, _downloader));
        if !writer.is_done() {
            let _ = timeout(Duration::from_secs(1), &mut writer).await;
        }
    }
}

/// The `id` of a request (0 for `hello`).
fn request_id(msg: &FilesViewerMsg) -> u32 {
    match msg {
        FilesViewerMsg::Hello { .. } | FilesViewerMsg::Unknown => 0,
        FilesViewerMsg::List { id, .. }
        | FilesViewerMsg::Stat { id, .. }
        | FilesViewerMsg::Mkdir { id, .. }
        | FilesViewerMsg::Rename { id, .. }
        | FilesViewerMsg::Delete { id, .. }
        | FilesViewerMsg::Read { id, .. }
        | FilesViewerMsg::Write { id, .. }
        | FilesViewerMsg::Cancel { id } => *id,
    }
}

/// The queueing a transfer may cause: gentle while a screen session is open.
fn target_for(state: &AgentState) -> u64 {
    if state.in_session {
        TARGET_GENTLE_US
    } else {
        TARGET_FULL_US
    }
}

async fn write_loop(
    mut sink: SplitSink<WsStream, Message>,
    mut control: mpsc::UnboundedReceiver<Message>,
    mut data: mpsc::Receiver<Message>,
) {
    loop {
        let msg = tokio::select! {
            biased;
            msg = control.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            Some(msg) = data.recv() => msg,
        };
        let closing = matches!(msg, Message::Close(_));
        if let Err(e) = sink.send(msg).await {
            debug!("file channel write failed: {e}");
            return;
        }
        if closing {
            return;
        }
    }
    let _ = sink.close().await;
}
