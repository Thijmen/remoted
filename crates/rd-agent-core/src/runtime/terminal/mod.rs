//! Terminal channels (docs/terminal.md): a shell in a pseudo terminal, shown by a viewer on a
//! socket of its own (`/ws/agent/terminal/{channel}`), next to and independent of the screen
//! session and the file channels.
//!
//! ```text
//! Channels      at most 16 at a time; stopped when the runtime shuts down
//!   └─ channel  one task per socket: hello (new shell or attach), TERM_IN, resize, close,
//!        │      TERM_ACK, PING; sends the shell's history from its cursor through a BulkWindow
//!        └─ writer      sole owner of the sink; control (hello, PONG, state, err) jumps ahead
//!                       of the output queue (TERM_OUT, then `exit` and the close after it)
//! shells.rs     the shells, which outlive channels: reader / writer / waiter threads, keep timer
//! history.rs    the last 1 MiB of a shell's output, by absolute offset
//! ```
//!
//! A shell's reader thread always drains the terminal into its history, attached or not, so a
//! slow or absent viewer never blocks the shell. The channel sends from the history as the
//! window allows: at once when nothing is in flight (a keystroke's echo never waits), at most
//! the window while a command prints a lot. Input goes to the shell's writer thread through a
//! bounded queue; while it is full the channel stops reading its socket.

mod history;
mod shells;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use rd_flow::{BulkConfig, BulkWindow, TARGET_FULL_US, TARGET_GENTLE_US};
use rd_proto::bin::{self, BinMsg, TermOutHeader};
use rd_proto::close;
use rd_proto::control::AgentState;
use rd_proto::terminal::{
    MAX_MESSAGE, MAX_OUT_CHUNK, MAX_SIZE, MIN_SIZE, TERMINAL_PROTO_VERSION, TermAgentMsg,
    TermViewerMsg, detach, err,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{Instrument as _, debug, info, warn};

use super::Ctx;
use super::net::{WsStream, valid_session_id};
use super::task::TaskGuard;
use crate::clock::now_us;
use crate::platform::{PtyExit, PtySize};
use shells::{Attachment, Shell, StartError};

pub(crate) use shells::Shells;

/// Terminal channels at a time. The gateway allows 8 per machine and frees a slot as soon as
/// the viewer's side ends, while this side may still be closing; the margin keeps such a
/// channel from blocking a new one. This cap only guards against a gateway that does not limit.
const MAX_CHANNELS: usize = 16;
/// How long shutdown waits for a channel to close politely.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// The terminal channels of the runtime.
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
            warn!(channel = id, "too many terminal channels; request ignored");
            return;
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        let span = tracing::info_span!("terminal", id = %&id[..id.len().min(8)]);
        let task = TaskGuard::spawn(run(self.ctx.clone(), id, user, stop_rx).instrument(span));
        self.running.push(Running {
            stop: Some(stop_tx),
            task,
        });
    }

    /// Closes every channel (politely, within a grace period). The shells are not ended here
    /// ([`Shells::shutdown`]).
    pub(crate) async fn shutdown(&mut self) {
        for r in &mut self.running {
            if let Some(stop) = r.stop.take() {
                let _ = stop.send(());
            }
        }
        for mut r in self.running.drain(..) {
            if timeout(SHUTDOWN_GRACE, &mut r.task).await.is_err() {
                debug!("a terminal channel did not close in time; aborted");
            }
        }
    }
}

/// How a channel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The viewer closed it.
    Closed,
    Failed,
    Idle,
    HelloTimeout,
    Protocol,
    /// `hello` was refused (`err` sent).
    Refused,
    /// The shell ended; `exit` and the close are queued behind its last output.
    Exited,
    /// Another channel attached the shell.
    Kicked,
    Shutdown,
}

impl End {
    fn close_code(self) -> u16 {
        match self {
            End::Protocol | End::HelloTimeout => close::PROTOCOL,
            End::Kicked => close::KICKED,
            End::Shutdown => 1001,
            _ => 1000,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            End::Protocol => "protocol error",
            End::HelloTimeout => "no hello",
            End::Kicked => "taken over",
            End::Shutdown => "agent shutting down",
            End::Exited => "shell exited",
            _ => "closed",
        }
    }
}

async fn run(ctx: Arc<Ctx>, id: String, user: String, mut stop: oneshot::Receiver<()>) {
    let url = ctx.gateway.terminal_url(&id);
    let dial = timeout(
        ctx.timings.dial_timeout,
        ctx.gateway.dial(&url, MAX_MESSAGE),
    );
    let ws = tokio::select! {
        _ = &mut stop => return,
        r = dial => match r {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                warn!("dialing the terminal channel failed: {e:#}");
                return;
            }
            Err(_) => {
                warn!("dialing the terminal channel timed out");
                return;
            }
        },
    };
    debug!(user, "terminal: channel open");
    let end = Channel::start(ctx, ws, user.clone()).run(&mut stop).await;
    debug!(user, end = ?end, "terminal: channel closed");
}

/// The shell a channel shows.
struct Attached {
    shell: Arc<Shell>,
    generation: u64,
    /// Notified when the shell has new output or exited.
    wake: Arc<Notify>,
    /// Offset of the next output byte to send.
    cursor: u64,
    input: mpsc::Sender<Bytes>,
}

struct Channel {
    ctx: Arc<Ctx>,
    user: String,
    control: mpsc::UnboundedSender<Message>,
    /// TERM_OUT, then `exit` and the close: in order. Unbounded, but the window bounds what is
    /// in it (queued frames count as in flight, and their delay shrinks the window).
    output: mpsc::UnboundedSender<Message>,
    writer: TaskGuard<()>,
    stream: SplitStream<WsStream>,
    window: BulkWindow,
    attached: Option<Attached>,
    /// Fires when another channel takes the shell over.
    kicked: Option<oneshot::Receiver<()>>,
    /// A TERM_IN frame waiting for room in the shell's input queue.
    pending_input: Option<Bytes>,
}

impl Channel {
    fn start(ctx: Arc<Ctx>, ws: WsStream, user: String) -> Channel {
        let (sink, stream) = ws.split();
        let (control, control_rx) = mpsc::unbounded_channel();
        let (output, output_rx) = mpsc::unbounded_channel();
        let writer = TaskGuard::spawn(write_loop(sink, control_rx, output_rx));
        let window = BulkWindow::new(BulkConfig::default(), target_for(&ctx.state.borrow()));
        Channel {
            ctx,
            user,
            control,
            output,
            writer,
            stream,
            window,
            attached: None,
            kicked: None,
            pending_input: None,
        }
    }

    async fn run(mut self, stop: &mut oneshot::Receiver<()>) -> End {
        let mut state = self.ctx.state.subscribe();
        state.mark_unchanged();
        let hello_deadline = tokio::time::sleep(self.ctx.timings.hello_timeout);
        tokio::pin!(hello_deadline);
        let idle = self.ctx.timings.idle_timeout;
        let end = loop {
            if let Some(exit) = self.pump() {
                self.send_exit(exit);
                break End::Exited;
            }
            let attached = self.attached.is_some();
            let reading = self.pending_input.is_none();
            tokio::select! {
                biased;
                _ = &mut *stop => break End::Shutdown,
                _ = &mut self.writer => break End::Failed,
                Ok(()) = kicked(&mut self.kicked), if attached => break End::Kicked,
                permit = reserve(&self.attached), if !reading => {
                    if let (Some(permit), Some(bytes)) = (permit, self.pending_input.take()) {
                        permit.send(bytes);
                    }
                }
                msg = timeout(idle, self.stream.next()), if reading => {
                    let msg = match msg {
                        Err(_) => break End::Idle,
                        Ok(None) => break End::Closed,
                        Ok(Some(Err(e))) => {
                            debug!("terminal channel failed: {e}");
                            break End::Failed;
                        }
                        Ok(Some(Ok(msg))) => msg,
                    };
                    let step = match msg {
                        Message::Text(text) => self.on_text(text.as_str()).await,
                        Message::Binary(data) => {
                            self.on_binary(data);
                            Ok(())
                        }
                        Message::Close(_) => Err(End::Closed),
                        _ => Ok(()),
                    };
                    if let Err(end) = step {
                        break end;
                    }
                }
                () = woken(&self.attached), if attached => {}
                Ok(()) = state.changed() => {
                    let s = *state.borrow_and_update();
                    self.window.set_target(target_for(&s));
                    if attached {
                        self.json(&TermAgentMsg::State(s));
                    }
                }
                () = &mut hello_deadline, if !attached => {
                    warn!("the viewer did not say hello on the terminal channel");
                    break End::HelloTimeout;
                }
            }
        };
        self.close(end).await;
        end
    }

    /// Sends the shell's output as far as the window allows. Returns the exit once everything
    /// up to it was sent.
    fn pump(&mut self) -> Option<PtyExit> {
        let a = self.attached.as_mut()?;
        loop {
            let chunk = (self.window.chunk_size() as usize).min(MAX_OUT_CHUNK);
            let pending = a.shell.pending(a.cursor, chunk);
            if pending.len == 0 {
                a.cursor = pending.offset;
                return pending.exit;
            }
            if !self.window.can_send(pending.len as u64) {
                return None;
            }
            let (offset, bytes) = a.shell.read(pending.offset, pending.len);
            if bytes.is_empty() {
                return None;
            }
            if offset > a.cursor {
                debug!(
                    skipped = offset - a.cursor,
                    "terminal: the history overtook the channel"
                );
            }
            let seq = self.window.on_sent(bytes.len() as u64, now_us());
            a.cursor = offset + bytes.len() as u64;
            let frame = TermOutHeader { seq, offset }.to_message(&bytes);
            let _ = self.output.send(Message::Binary(frame.into()));
        }
    }

    fn json(&self, msg: &TermAgentMsg) {
        match serde_json::to_string(msg) {
            Ok(text) => {
                let _ = self.control.send(Message::Text(text.into()));
            }
            Err(e) => warn!("serializing a terminal message: {e}"),
        }
    }

    fn err(&self, code: &str, msg: impl Into<String>) {
        self.json(&TermAgentMsg::Err {
            code: code.into(),
            msg: msg.into(),
        });
    }

    /// `exit` behind the last output, then the close; the shell is forgotten.
    fn send_exit(&mut self, exit: PtyExit) {
        let msg = TermAgentMsg::Exit {
            code: exit.code,
            signal: exit.signal,
        };
        if let Ok(text) = serde_json::to_string(&msg) {
            let _ = self.output.send(Message::Text(text.into()));
        }
        let _ = self.output.send(close_message(End::Exited));
        if let Some(a) = self.attached.take() {
            self.ctx.shells.remove(&a.shell);
        }
    }

    fn on_binary(&mut self, data: Bytes) {
        match bin::parse(&data) {
            Ok(BinMsg::TermIn(bytes)) => {
                if bytes.is_empty() {
                    return;
                }
                let Some(a) = &self.attached else {
                    debug!("terminal input before hello ignored");
                    return;
                };
                let bytes = data.slice(bin::TERM_IN_HEADER_LEN..);
                match a.input.try_send(bytes) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(bytes)) => {
                        debug!("terminal: the shell is not reading its input; holding back");
                        self.pending_input = Some(bytes);
                    }
                    // The shell does not read input any more (it is exiting).
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
            Ok(BinMsg::TermAck(ack)) => {
                self.window.on_ack(ack.seq, now_us());
            }
            Ok(BinMsg::Ping(ping)) => {
                let pong = ping.pong(now_us()).to_message();
                let _ = self.control.send(Message::Binary(pong.into()));
            }
            Ok(_) => {}
            Err(e) => debug!("malformed binary message on a terminal channel: {e}"),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), End> {
        let msg = match serde_json::from_str::<TermViewerMsg>(text) {
            Ok(msg) => msg,
            Err(e) if self.attached.is_none() => {
                self.err(err::BAD_REQUEST, format!("invalid message: {e}"));
                return Err(End::Refused);
            }
            Err(e) => {
                debug!("invalid terminal message ignored: {e}");
                return Ok(());
            }
        };
        match msg {
            TermViewerMsg::Hello {
                proto,
                cols,
                rows,
                attach,
                offset,
            } => {
                if self.attached.is_some() {
                    debug!("duplicate hello ignored");
                    return Ok(());
                }
                self.on_hello(proto, clamp_size(cols, rows), attach, offset)
                    .await
            }
            TermViewerMsg::Unknown => {
                debug!("unknown terminal message ignored");
                Ok(())
            }
            _ if self.attached.is_none() => {
                self.err(err::BAD_REQUEST, "send hello first");
                Err(End::Refused)
            }
            TermViewerMsg::Resize { cols, rows } => {
                if let Some(a) = &self.attached {
                    a.shell.resize(clamp_size(cols, rows));
                }
                Ok(())
            }
            TermViewerMsg::Close => {
                if let Some(a) = &self.attached {
                    info!(
                        shell = a.shell.short(),
                        user = self.user,
                        "terminal: shell closed by the viewer"
                    );
                    a.shell.hang_up();
                }
                Ok(())
            }
        }
    }

    async fn on_hello(
        &mut self,
        proto: u16,
        size: PtySize,
        attach: Option<String>,
        offset: u64,
    ) -> Result<(), End> {
        if proto < TERMINAL_PROTO_VERSION {
            self.err(
                err::PROTO_MISMATCH,
                format!(
                    "the viewer speaks terminal protocol {proto}, the agent {TERMINAL_PROTO_VERSION}"
                ),
            );
            return Err(End::Protocol);
        }
        let shells = self.ctx.shells.clone();
        let (shell, offset, reset) = match attach {
            Some(id) => {
                let found = valid_session_id(&id).then(|| shells.get(&id)).flatten();
                let Some(shell) = found else {
                    self.err(err::NOT_FOUND, "this shell is gone");
                    return Err(End::Refused);
                };
                shell.resize(size);
                let (offset, reset) = shell.resume(offset);
                (shell, offset, reset)
            }
            None => match shells.start(&self.user, size).await {
                Ok(shell) => (shell, 0, true),
                Err(StartError::Busy) => {
                    self.err(
                        err::BUSY,
                        format!("the agent already runs {} shells", shells::MAX_SHELLS),
                    );
                    return Err(End::Refused);
                }
                Err(StartError::Spawn(msg)) => {
                    self.err(err::SPAWN, msg);
                    return Err(End::Refused);
                }
            },
        };
        let Attachment {
            generation,
            wake,
            kicked,
        } = shell.attach(&self.user);
        self.json(&TermAgentMsg::Hello {
            proto: TERMINAL_PROTO_VERSION,
            id: shell.id.clone(),
            os: self.ctx.platform.os().into(),
            shell: shell.name.clone(),
            offset,
            reset,
            state: *self.ctx.state.borrow(),
        });
        self.kicked = Some(kicked);
        self.attached = Some(Attached {
            input: shell.input(),
            shell,
            generation,
            wake,
            cursor: offset,
        });
        Ok(())
    }

    async fn close(mut self, end: End) {
        match end {
            // Already queued behind the output.
            End::Exited => {}
            End::Kicked => {
                self.json(&TermAgentMsg::Detached {
                    reason: detach::TAKEN_OVER.into(),
                });
                let _ = self.control.send(close_message(end));
            }
            _ => {
                let _ = self.control.send(close_message(end));
            }
        }
        if let Some(a) = self.attached.take() {
            a.shell.detach(a.generation, &self.user);
        }
        let Channel {
            control,
            output,
            mut writer,
            ..
        } = self;
        drop((control, output));
        if !writer.is_done() {
            let _ = timeout(Duration::from_secs(1), &mut writer).await;
        }
    }
}

fn close_message(end: End) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::from(end.close_code()),
        reason: end.reason().into(),
    }))
}

/// Resolves when another channel took the attached shell over.
async fn kicked(
    kicked: &mut Option<oneshot::Receiver<()>>,
) -> Result<(), oneshot::error::RecvError> {
    match kicked {
        Some(rx) => rx.await,
        None => std::future::pending().await,
    }
}

/// Resolves when the attached shell has new output or exited.
async fn woken(attached: &Option<Attached>) {
    match attached {
        Some(a) => a.wake.notified().await,
        None => std::future::pending().await,
    }
}

/// Room for one frame in the attached shell's input queue (`None`: the shell is gone).
async fn reserve(attached: &Option<Attached>) -> Option<mpsc::OwnedPermit<Bytes>> {
    match attached {
        Some(a) => a.input.clone().reserve_owned().await.ok(),
        None => std::future::pending().await,
    }
}

fn clamp_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(MIN_SIZE, MAX_SIZE),
        rows: rows.clamp(MIN_SIZE, MAX_SIZE),
    }
}

/// The queueing the output may cause: gentle while a screen session is open.
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
    mut output: mpsc::UnboundedReceiver<Message>,
) {
    loop {
        let msg = tokio::select! {
            biased;
            msg = control.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            Some(msg) = output.recv() => msg,
        };
        let closing = matches!(msg, Message::Close(_));
        if let Err(e) = sink.send(msg).await {
            debug!("terminal channel write failed: {e}");
            return;
        }
        if closing {
            return;
        }
    }
    // The channel is done: what is still queued behind it (output, `exit`, the close) goes
    // out first.
    while let Ok(msg) = output.try_recv() {
        let closing = matches!(msg, Message::Close(_));
        if sink.send(msg).await.is_err() || closing {
            return;
        }
    }
    let _ = sink.close().await;
}
