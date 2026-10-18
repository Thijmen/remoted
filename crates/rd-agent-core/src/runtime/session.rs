//! One viewer session on its own data socket (`/ws/agent/session/{id}`).
//!
//! Tasks per session:
//! - **reader**: parses everything the viewer sends. VIDEO_ACK, PONG, PING and input are
//!   handled right there (ACKs feed the flow window, PINGs are answered at once, input goes to
//!   the input thread), so they never wait behind video. JSON goes to the session task.
//! - **writer**: owns the socket's sink. Two queues: control (PONG, PING, JSON) is always
//!   drained before video, and video (`video_config` + VIDEO) is short because the flow window
//!   already bounds it.
//! - **video sender** ([`super::video`]): encoded frames -> video queue.
//! - **session task** (this file): hello/start/set/pause/..., bitrate/ping/stats ticks, stall
//!   detection, teardown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use rd_proto::bin::{self, BinMsg, Ping};
use rd_proto::control::{
    AgentInfo, AgentMsg, AgentStats, StreamParams, StreamParamsPatch, ViewerInfo, ViewerMsg,
};
use rd_proto::{MIN_PROTO_VERSION, PROTO_VERSION, close};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Interval, MissedTickBehavior, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{Instrument as _, debug, error, info, warn};

use super::flow::{Effective, Flow, MAX_WINDOW_FRAMES};
use super::input::{InputThread, OnLockedDrop};
use super::net::WsStream;
use super::task::TaskGuard;
use super::video::{self, EncodeWindow};
use super::{AGENT_VERSION, ActiveSession, Ctx};
use crate::clock::now_us;
use crate::platform::{FrameGate, Guard, InputEvent, Platform, VideoPipeline};

/// Control messages (PONG, PING, JSON, close) waiting for the writer.
const CONTROL_QUEUE: usize = 32;
/// Video messages waiting for the writer; small, the flow window already bounds video.
const VIDEO_QUEUE: usize = 4;
/// Encoded frames from the pipeline (the [`crate::platform::FrameSink`] contract).
///
/// A full sink forces a post-encode drop (and an IDR), so it must hold every frame the window
/// can have in flight: while the socket is blocked the writer holds one message, the video
/// queue four (a `video_config` takes a slot too), the sender one, and the rest of the window
/// (up to [`MAX_WINDOW_FRAMES`] = 16) piles up here. 8 slots were not enough for a 16-frame
/// window (14 frames on the agent side). Twice the window also covers a window reset on resume,
/// which admits a new window while frames of the old one may still be queued.
const FRAME_SINK_CAPACITY: usize = 2 * MAX_WINDOW_FRAMES as usize;
/// Viewer JSON messages waiting for the session task.
const VIEWER_QUEUE: usize = 32;
/// Largest message accepted from the viewer (the gateway itself allows 64 KiB).
pub(crate) const MAX_MESSAGE: usize = 1 << 20;

const BITRATE_TICK: Duration = Duration::from_millis(250);
const PING_INTERVAL: Duration = Duration::from_millis(500);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// How often the pipeline is asked whether capture stopped on its own.
const CAPTURE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// PONGs claiming a longer round trip are bogus.
const MAX_RTT_US: u64 = 60_000_000;
/// A new session waits this long for its predecessor to release capture and input.
const PREDECESSOR_GRACE: Duration = Duration::from_secs(5);
/// Budget for queueing the close frame and for the writer to flush it.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Why the session manager stops a session.
#[derive(Debug)]
pub(crate) enum StopReason {
    /// A newer `session_request` replaces this session.
    Replaced,
    /// The gateway sent `session_end` (with its reason).
    Gateway(String),
    /// The agent is shutting down.
    Shutdown,
}

/// A session that ended on the agent's initiative; reported with `session_end`.
#[derive(Debug)]
pub(crate) struct Ended {
    pub(crate) session: String,
    pub(crate) reason: &'static str,
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    DialFailed,
    ViewerClosed,
    SocketError,
    Idle,
    HelloTimeout,
    ProtoMismatch,
    Stall,
    Replaced,
    Gateway,
    Shutdown,
}

impl End {
    fn reason(self) -> &'static str {
        match self {
            End::DialFailed => "dial_failed",
            End::ViewerClosed => "viewer_closed",
            End::SocketError => "socket_error",
            End::Idle => "idle",
            End::HelloTimeout => "hello_timeout",
            End::ProtoMismatch => "proto_mismatch",
            End::Stall => "stall",
            End::Replaced => "replaced",
            End::Gateway => "gateway",
            End::Shutdown => "agent_shutdown",
        }
    }

    fn close_code(self) -> u16 {
        match self {
            End::ProtoMismatch => close::PROTOCOL,
            End::Stall | End::HelloTimeout | End::Idle | End::SocketError => close::SESSION_ENDED,
            End::Shutdown => 1001,
            End::DialFailed | End::ViewerClosed | End::Replaced | End::Gateway => 1000,
        }
    }

    /// The gateway already knows about replacements and its own `session_end`s.
    fn notify_gateway(self) -> bool {
        !matches!(self, End::Replaced | End::Gateway)
    }

    fn from_stop(r: Result<StopReason, oneshot::error::RecvError>) -> End {
        match r {
            Ok(StopReason::Replaced) => End::Replaced,
            Ok(StopReason::Gateway(reason)) => {
                info!(reason, "gateway ended the session");
                End::Gateway
            }
            // The manager is gone: the runtime is going away.
            Ok(StopReason::Shutdown) | Err(_) => End::Shutdown,
        }
    }

    fn from_read(r: Result<ReadEnd, tokio::task::JoinError>) -> End {
        match r {
            Ok(ReadEnd::Closed) => End::ViewerClosed,
            Ok(ReadEnd::Idle) => {
                warn!("nothing received on the session socket; giving up");
                End::Idle
            }
            Ok(ReadEnd::Failed(e)) => {
                info!("session socket failed: {e}");
                End::SocketError
            }
            Err(e) => {
                error!("session reader failed: {e}");
                End::SocketError
            }
        }
    }
}

/// Entry point of a session task: waits for `predecessor` to clean up, dials the data socket,
/// runs the session and reports agent-initiated ends through `ended`.
pub(crate) async fn run(
    ctx: Arc<Ctx>,
    id: String,
    user: String,
    mut stop: oneshot::Receiver<StopReason>,
    predecessor: Option<TaskGuard<()>>,
    ended: mpsc::UnboundedSender<Ended>,
) {
    let span = tracing::info_span!("session", id = %&id[..id.len().min(8)]);
    async move {
        if let Some(prev) = predecessor
            && timeout(PREDECESSOR_GRACE, prev).await.is_err()
        {
            warn!("previous session did not finish cleaning up; aborted it");
        }
        let (end, active) = connect_and_run(&ctx, &id, &user, &mut stop).await;
        info!(reason = end.reason(), "session ended");
        if end.notify_gateway() {
            let _ = ended.send(Ended {
                session: id,
                reason: end.reason(),
            });
        }
        // `in_session` turns false only after `session_end` is queued for the gateway.
        drop(active);
    }
    .instrument(span)
    .await
}

/// Returns how the session ended and, if it got connected, its [`ActiveSession`] mark.
async fn connect_and_run(
    ctx: &Arc<Ctx>,
    id: &str,
    user: &str,
    stop: &mut oneshot::Receiver<StopReason>,
) -> (End, Option<ActiveSession>) {
    let url = ctx.gateway.session_url(id);
    let dial = timeout(
        ctx.timings.dial_timeout,
        ctx.gateway.dial(&url, MAX_MESSAGE),
    );
    let ws = tokio::select! {
        r = &mut *stop => return (End::from_stop(r), None),
        r = dial => match r {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                warn!("dialing the session socket failed: {e:#}");
                return (End::DialFailed, None);
            }
            Err(_) => {
                warn!("dialing the session socket timed out");
                return (End::DialFailed, None);
            }
        },
    };
    info!(user, "session socket connected");
    let active = ctx.session_active();
    let end = Session::start(ctx.clone(), ws).await.run(stop).await;
    (end, Some(active))
}

/// Non-blocking sender for the writer's control queue.
#[derive(Clone)]
struct Outbox(mpsc::Sender<Message>);

impl Outbox {
    fn json(&self, msg: &AgentMsg) {
        match serde_json::to_string(msg) {
            Ok(text) => self.push(Message::Text(text.into())),
            Err(e) => warn!("serializing an agent message: {e}"),
        }
    }

    fn error(&self, code: &str, msg: impl Into<String>) {
        let msg = msg.into();
        warn!(code, msg, "reporting an error to the viewer");
        self.json(&AgentMsg::Error {
            code: code.into(),
            msg,
        });
    }

    fn binary(&self, data: Vec<u8>) {
        self.push(Message::Binary(data.into()));
    }

    fn push(&self, msg: Message) {
        // The writer is stuck behind a blocked socket (stall detection deals with that) or
        // already gone (the session is ending).
        if let Err(TrySendError::Full(_)) = self.0.try_send(msg) {
            warn!("session control queue full; message dropped");
        }
    }
}

/// What the reader passes to the session task.
#[derive(Debug)]
enum FromViewer {
    Msg(ViewerMsg),
    /// A message that did not parse, e.g. a `start` with an unknown codec: answered with
    /// `bad_request` (in order with the other replies).
    Invalid {
        /// Its `t`, if it has one.
        t: Option<String>,
        error: String,
    },
}

/// State the reader needs from the session.
#[derive(Default)]
struct Shared {
    /// Set by the first `start`; lives as long as the session.
    flow: OnceLock<Arc<Flow>>,
    /// Teardown started: the reader only waits for the close handshake.
    closing: AtomicBool,
}

struct Session {
    ctx: Arc<Ctx>,
    out: Outbox,
    /// Cloned into the video sender.
    video_out: mpsc::Sender<Message>,
    writer: TaskGuard<()>,
    reader: TaskGuard<ReadEnd>,
    viewer: mpsc::Receiver<FromViewer>,
    shared: Arc<Shared>,
    input: Option<InputThread>,
    /// Why input injection is unavailable, reported after `hello`.
    input_error: Option<String>,
    /// Negotiated protocol version, once the viewer said hello.
    proto: Option<u16>,
    stream: Option<Streaming>,
    paused: bool,
    keyframes: KeyframeLimiter,
    timers: Timers,
}

impl Session {
    async fn start(ctx: Arc<Ctx>, ws: WsStream) -> Session {
        let (sink, stream) = ws.split();
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (video_out, video_rx) = mpsc::channel(VIDEO_QUEUE);
        let (viewer_tx, viewer) = mpsc::channel(VIEWER_QUEUE);
        let writer = TaskGuard::spawn(write_loop(sink, control_rx, video_rx));
        let out = Outbox(control_tx);

        let (input, input_error) = match spawn_input(
            ctx.platform.clone(),
            locked_reporter(ctx.clone(), out.clone()),
        )
        .await
        {
            Ok(input) => (Some(input), None),
            Err(e) => {
                error!("input injection unavailable: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        };
        let shared = Arc::new(Shared::default());
        let reader = TaskGuard::spawn(read_loop(
            stream,
            Reader {
                shared: shared.clone(),
                out: out.clone(),
                input: input.as_ref().map(InputThread::sender),
                viewer: viewer_tx,
                idle: ctx.timings.idle_timeout,
            },
        ));
        Session {
            ctx,
            out,
            video_out,
            writer,
            reader,
            viewer,
            shared,
            input,
            input_error,
            proto: None,
            stream: None,
            paused: false,
            keyframes: KeyframeLimiter::default(),
            timers: Timers::new(),
        }
    }

    async fn run(mut self, stop: &mut oneshot::Receiver<StopReason>) -> End {
        let mut state = self.ctx.state.subscribe();
        state.mark_unchanged();
        let hello_deadline = tokio::time::sleep(self.ctx.timings.hello_timeout);
        tokio::pin!(hello_deadline);
        let end = loop {
            let streaming = self.stream.is_some();
            let awaiting_hello = self.proto.is_none();
            let step = tokio::select! {
                biased;
                r = &mut *stop => Err(End::from_stop(r)),
                r = &mut self.reader => Err(End::from_read(r)),
                _ = &mut self.writer => Err(End::SocketError),
                msg = self.viewer.recv() => match msg {
                    Some(msg) => self.on_viewer_msg(msg).await,
                    None => Err(End::ViewerClosed),
                },
                _ = self.timers.bitrate.tick(), if streaming => self.on_bitrate_tick(),
                _ = self.timers.ping.tick(), if streaming => {
                    self.send_ping();
                    Ok(())
                }
                _ = self.timers.stats.tick(), if streaming => {
                    self.send_stats();
                    Ok(())
                }
                _ = self.timers.capture.tick(), if streaming => {
                    self.check_capture();
                    Ok(())
                }
                Ok(()) = state.changed() => {
                    let s = *state.borrow_and_update();
                    if !awaiting_hello {
                        self.out.json(&AgentMsg::State(s));
                    }
                    Ok(())
                }
                () = &mut hello_deadline, if awaiting_hello => {
                    warn!("the viewer did not say hello in time");
                    Err(End::HelloTimeout)
                }
            };
            if let Err(end) = step {
                break end;
            }
        };
        self.close(end).await;
        end
    }

    async fn on_viewer_msg(&mut self, msg: FromViewer) -> Result<(), End> {
        let msg = match msg {
            FromViewer::Msg(msg) => msg,
            FromViewer::Invalid { t, error } => {
                let what = t.map_or("unparseable message".to_owned(), |t| {
                    format!("invalid {t:?} message")
                });
                self.out.error("bad_request", format!("{what}: {error}"));
                return Ok(());
            }
        };
        match msg {
            ViewerMsg::Hello {
                proto,
                min_proto,
                viewer,
            } => return self.on_hello(proto, min_proto, &viewer),
            ViewerMsg::Unknown => {}
            _ if self.proto.is_none() => self.out.error("bad_request", "send hello first"),
            ViewerMsg::Start(params) => self.on_start(params).await,
            ViewerMsg::Set(patch) => self.on_set(patch).await,
            ViewerMsg::KeyframeReq { reason } => {
                debug!(reason, "keyframe requested");
                self.request_keyframe();
            }
            ViewerMsg::Pause => self.set_paused(true),
            ViewerMsg::Resume => self.set_paused(false),
        }
        Ok(())
    }

    fn on_hello(&mut self, proto: u16, min_proto: u16, viewer: &ViewerInfo) -> Result<(), End> {
        if self.proto.is_some() {
            debug!("duplicate hello ignored");
            return Ok(());
        }
        let Some(version) = negotiate(proto, min_proto) else {
            self.out.error(
                "proto_mismatch",
                format!(
                    "viewer speaks protocol {min_proto}..={proto}, agent \
                     {MIN_PROTO_VERSION}..={PROTO_VERSION}"
                ),
            );
            return Err(End::ProtoMismatch);
        };
        self.proto = Some(version);
        info!(
            proto = version,
            ua = viewer.ua,
            dpr = viewer.dpr,
            viewport = ?viewer.viewport,
            "viewer hello"
        );
        let platform = &self.ctx.platform;
        self.out.json(&AgentMsg::Hello {
            proto: version,
            agent: AgentInfo {
                os: platform.os().into(),
                version: AGENT_VERSION.into(),
                displays: platform.displays(),
            },
            caps: Vec::new(),
        });
        self.out.json(&AgentMsg::State(*self.ctx.state.borrow()));
        if let Some(e) = &self.input_error {
            self.out
                .error("internal", format!("input unavailable: {e}"));
        } else if !platform.permissions().input {
            self.out.error(
                "tcc_input",
                "the agent has no Accessibility permission; input is ignored",
            );
        }
        Ok(())
    }

    async fn on_start(&mut self, requested: StreamParams) {
        let params = match effective(&self.ctx, &requested) {
            Ok(params) => params,
            Err(msg) => return self.out.error("bad_request", msg),
        };
        if let Some(stream) = self.stream.as_mut() {
            info!("start on a running stream: restarting it");
            stream.flow.restart(&params);
            stream.requested = requested;
            let result = stream.reconfigure(params).await;
            self.timers.restart();
            if let Err(e) = result {
                self.out
                    .error("internal", format!("restarting video: {e:#}"));
            }
            return;
        }
        // The preflight can say no while capture works (it caches a stale answer), so it only
        // decides how a failed start is reported.
        let screen_ok = self.ctx.platform.permissions().screen;
        if !screen_ok {
            warn!("Screen Recording preflight says no; trying capture anyway");
        }
        match self.start_stream(requested, params).await {
            Ok(stream) => {
                self.stream = Some(stream);
                self.timers.restart();
            }
            Err(e) if !screen_ok => self.out.error(
                "tcc_screen",
                format!("the agent has no Screen Recording permission ({e:#})"),
            ),
            Err(e) => self.out.error("internal", format!("starting video: {e:#}")),
        }
    }

    async fn start_stream(
        &mut self,
        requested: StreamParams,
        params: Effective,
    ) -> anyhow::Result<Streaming> {
        let flow = match self.shared.flow.get() {
            // An earlier start failed after creating the flow.
            Some(flow) => {
                flow.restart(&params);
                // A new pipeline: its first frame must be a keyframe, like a new flow's.
                flow.gate.request_keyframe();
                flow.clone()
            }
            None => self
                .shared
                .flow
                .get_or_init(|| Arc::new(Flow::new(&params)))
                .clone(),
        };
        flow.set_paused(self.paused);
        let platform = self.ctx.platform.clone();
        let mut pipeline = Pipeline::create(platform.clone()).await?;
        let (sink, frames) = mpsc::channel(FRAME_SINK_CAPACITY);
        let gate: Arc<dyn FrameGate> = flow.clone();
        let config = params.video_config(flow.target_bps());
        pipeline
            .call(move |p| p.start(config, gate, sink))
            .await?
            .context("starting the video pipeline")?;
        let awake = platform.keep_awake("rd remote session");
        platform.declare_user_activity();
        let encode = Arc::new(Mutex::new(EncodeWindow::default()));
        let sender = TaskGuard::spawn(video::send_video(
            frames,
            flow.clone(),
            self.video_out.clone(),
            encode.clone(),
        ));
        info!(
            display = params.display,
            width = params.width,
            height = params.height,
            fps = params.fps,
            start_kbps = params.bitrate.start_bps / 1000,
            max_kbps = params.bitrate.max_bps / 1000,
            adaptive = params.bitrate.adaptive,
            "video started"
        );
        Ok(Streaming {
            requested,
            params,
            mark: StatsMark::new(&flow),
            flow,
            pipeline,
            sender,
            awake,
            encode,
            ping_id: 0,
            stalled: false,
            capture_failed: false,
        })
    }

    async fn on_set(&mut self, patch: StreamParamsPatch) {
        let Some(stream) = self.stream.as_mut() else {
            debug!("set before start ignored");
            return;
        };
        let mut requested = stream.requested.clone();
        apply_patch(&mut requested, &patch);
        let params = match effective(&self.ctx, &requested) {
            Ok(params) => params,
            Err(msg) => return self.out.error("bad_request", msg),
        };
        if params.bitrate != stream.params.bitrate {
            info!(
                start_kbps = params.bitrate.start_bps / 1000,
                min_kbps = params.bitrate.min_bps / 1000,
                max_kbps = params.bitrate.max_bps / 1000,
                adaptive = params.bitrate.adaptive,
                "bitrate limits changed"
            );
            stream.flow.bitrate.lock().set_config(params.bitrate);
        }
        stream.requested = requested;
        let result = if params.video_differs(&stream.params) {
            stream.flow.gate.set_fps(params.fps);
            stream.reconfigure(params).await
        } else {
            stream.params = params;
            Ok(())
        };
        if let Err(e) = result {
            self.out
                .error("internal", format!("reconfiguring video: {e:#}"));
        }
    }

    /// `keyframe_req` or resume. Without a stream there is nothing to do: a stream starts with
    /// a keyframe.
    fn request_keyframe(&mut self) {
        let Some(stream) = &self.stream else {
            return;
        };
        let on_its_way = stream.flow.gate.keyframe_on_its_way(now_us());
        if self.keyframes.request(Instant::now(), on_its_way) {
            stream.flow.gate.request_keyframe();
        } else if on_its_way {
            debug!("a keyframe is already on its way; request dropped");
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        info!(
            paused,
            "viewer {}",
            if paused { "paused" } else { "resumed" }
        );
        let Some(stream) = &self.stream else {
            return;
        };
        if !paused {
            // Frames left unacknowledged from before the pause will most likely never be
            // acknowledged; start the window over instead of stalling on them.
            let stale = stream
                .flow
                .gate
                .oldest_inflight_age_us(now_us())
                .is_some_and(|age| Duration::from_micros(age) > self.ctx.timings.stall_warn);
            if stale {
                info!("discarding stale in-flight frames from before the pause");
                stream.flow.gate.reset();
            }
        }
        stream.flow.set_paused(paused);
        if !paused {
            self.request_keyframe();
        }
    }

    fn on_bitrate_tick(&mut self) -> Result<(), End> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let now = now_us();
        let flow = &stream.flow;
        let rtt_min_us = flow.gate.stats().rtt_min_us;
        let (changed, target, decision) = {
            let mut bitrate = flow.bitrate.lock();
            let changed = bitrate.tick(now, rtt_min_us);
            (changed, bitrate.target_bps(), bitrate.last_decision())
        };
        flow.gate.set_target_bps(target);
        if let Some(bps) = changed {
            debug!(kbps = bps / 1000, ?decision, "encoder bitrate");
            stream.pipeline.set_bitrate(bps);
        }
        if self
            .keyframes
            .poll(Instant::now(), flow.gate.keyframe_on_its_way(now))
        {
            flow.gate.request_keyframe();
        }

        let age = flow
            .gate
            .oldest_inflight_age_us(now)
            .map_or(Duration::ZERO, Duration::from_micros);
        let timings = &self.ctx.timings;
        // A paused (hidden) viewer may legitimately sit on its frames.
        if age > timings.stall_end && !self.paused {
            error!(
                age_ms = age.as_millis(),
                "video stalled; ending the session"
            );
            return Err(End::Stall);
        }
        if age > timings.stall_warn {
            if !stream.stalled {
                let s = flow.gate.stats();
                warn!(
                    age_ms = age.as_millis(),
                    inflight_frames = s.inflight_frames,
                    inflight_bytes = s.inflight_bytes,
                    "video stalled: oldest in-flight frame not acknowledged"
                );
                stream.stalled = true;
            }
        } else if stream.stalled {
            info!("video stall cleared");
            stream.stalled = false;
        }
        Ok(())
    }

    fn send_ping(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        stream.ping_id = stream.ping_id.wrapping_add(1);
        let ping = Ping {
            id: stream.ping_id,
            t_origin_us: now_us(),
        };
        self.out.binary(ping.to_message());
    }

    fn send_stats(&mut self) {
        if let Some(stream) = self.stream.as_mut() {
            let stats = stream.stats();
            self.out.json(&AgentMsg::Stats(stats));
        }
    }

    /// Reports `capture_stopped` once when the pipeline says capture stopped by itself
    /// (permission revoked, display gone, ...). The session stays open: a later `set` or
    /// keyframe may get capture going again, and then a new failure is reported again.
    fn check_capture(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        match stream.pipeline.failure() {
            Some(reason) if !stream.capture_failed => {
                stream.capture_failed = true;
                error!(reason, "video capture stopped");
                self.out.json(&AgentMsg::Error {
                    code: "capture_stopped".into(),
                    msg: reason,
                });
            }
            None if stream.capture_failed => {
                stream.capture_failed = false;
                info!("video capture is running again");
            }
            _ => {}
        }
    }

    /// Teardown: input released first (never leave keys held), then capture stopped, the
    /// keep-awake guard dropped and the socket closed with `end`'s code.
    async fn close(mut self, end: End) {
        // From here on the reader ignores input and ACKs; it only sees the close handshake.
        self.shared.closing.store(true, Ordering::SeqCst);
        if let Some(input) = self.input.take() {
            input.finish().await;
        }
        if let Some(stream) = self.stream.take() {
            stream.stop().await;
        }
        let frame = CloseFrame {
            code: CloseCode::from(end.close_code()),
            reason: end.reason().into(),
        };
        let _ = timeout(CLOSE_GRACE, self.out.0.send(Message::Close(Some(frame)))).await;
        let Session {
            out,
            video_out,
            mut writer,
            mut reader,
            ..
        } = self;
        drop((out, video_out));
        if !writer.is_done() && timeout(CLOSE_GRACE, &mut writer).await.is_err() {
            debug!("session socket did not flush its close frame in time");
        }
        // Give the other side a moment to answer the close frame, then hang up.
        if !reader.is_done() {
            let _ = timeout(CLOSE_GRACE, &mut reader).await;
        }
        reader.stop().await;
    }
}

/// The video side of a started session.
struct Streaming {
    /// As requested by the viewer (base for `set` patches).
    requested: StreamParams,
    /// As applied.
    params: Effective,
    flow: Arc<Flow>,
    pipeline: Pipeline,
    sender: TaskGuard<()>,
    awake: Box<dyn Guard>,
    encode: Arc<Mutex<EncodeWindow>>,
    ping_id: u32,
    mark: StatsMark,
    stalled: bool,
    /// `capture_stopped` was reported and the pipeline still reports the failure.
    capture_failed: bool,
}

impl Streaming {
    /// Size/fps/display change: the pipeline emits a new codec config (new epoch).
    async fn reconfigure(&mut self, params: Effective) -> anyhow::Result<()> {
        let config = params.video_config(self.flow.target_bps());
        self.pipeline
            .call(move |p| p.reconfigure(config))
            .await?
            .context("reconfiguring the video pipeline")?;
        info!(
            display = params.display,
            width = params.width,
            height = params.height,
            fps = params.fps,
            "video reconfigured"
        );
        self.params = params;
        Ok(())
    }

    fn stats(&mut self) -> AgentStats {
        let now = Instant::now();
        let secs = now.duration_since(self.mark.at).as_secs_f32().max(1e-3);
        let flow = self.flow.gate.stats();
        let captured = self.flow.captured_total();
        let encode = self.encode.lock().take();
        let (target_bps, q_us) = {
            let bitrate = self.flow.bitrate.lock();
            (bitrate.target_bps(), bitrate.queue_delay_us())
        };
        let stats = AgentStats {
            target_kbps: target_bps / 1000,
            enc_kbps: (encode.bytes as f32 * 8.0 / 1000.0 / secs) as u32,
            cap_fps: captured.saturating_sub(self.mark.captured) as f32 / secs,
            enc_fps: encode.frames as f32 / secs,
            drop_window: saturating_u32(
                flow.drop_window_total.saturating_sub(self.mark.drop_window),
            ),
            drop_post: saturating_u32(flow.drop_post_total),
            enc_ms_p50: encode.enc_ms_p50,
            enc_ms_p99: encode.enc_ms_p99,
            rtt_min_ms: flow.rtt_min_us as f32 / 1000.0,
            q_ms: q_us as f32 / 1000.0,
            inflight_frames: flow.inflight_frames,
            inflight_bytes: saturating_u32(flow.inflight_bytes),
            window_frames: flow.window_frames,
            window_bytes: saturating_u32(flow.window_bytes),
            window_enc_ms: flow.encode_us as f32 / 1000.0,
        };
        self.mark = StatsMark {
            at: now,
            captured,
            drop_window: flow.drop_window_total,
        };
        stats
    }

    async fn stop(self) {
        let Streaming {
            pipeline,
            mut sender,
            awake,
            ..
        } = self;
        pipeline.stop().await;
        sender.stop().await;
        drop(awake);
        info!("video stopped");
    }
}

fn saturating_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Counters at the previous stats message.
struct StatsMark {
    at: Instant,
    captured: u64,
    drop_window: u64,
}

impl StatsMark {
    fn new(flow: &Flow) -> Self {
        StatsMark {
            at: Instant::now(),
            captured: flow.captured_total(),
            drop_window: flow.gate.stats().drop_window_total,
        }
    }
}

/// The platform pipeline. Its blocking calls run on the blocking pool; dropping it stops
/// capture.
struct Pipeline(Option<Box<dyn VideoPipeline>>);

impl Pipeline {
    async fn create(platform: Arc<dyn Platform>) -> anyhow::Result<Self> {
        let pipeline = tokio::task::spawn_blocking(move || platform.video_pipeline())
            .await
            .context("creating the video pipeline panicked")??;
        Ok(Pipeline(Some(pipeline)))
    }

    async fn call<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut dyn VideoPipeline) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let mut pipeline = self
            .0
            .take()
            .context("the video pipeline was lost after a panic")?;
        let (pipeline, result) = tokio::task::spawn_blocking(move || {
            let result = f(pipeline.as_mut());
            (pipeline, result)
        })
        .await
        .context("the video pipeline panicked")?;
        self.0 = Some(pipeline);
        Ok(result)
    }

    fn set_bitrate(&self, bps: u32) {
        if let Some(pipeline) = &self.0 {
            pipeline.set_bitrate(bps);
        }
    }

    fn failure(&self) -> Option<String> {
        self.0.as_ref().and_then(|p| p.failure())
    }

    async fn stop(mut self) {
        if let Some(mut pipeline) = self.0.take() {
            let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Some(mut pipeline) = self.0.take() {
            pipeline.stop();
        }
    }
}

struct Timers {
    bitrate: Interval,
    ping: Interval,
    stats: Interval,
    capture: Interval,
}

impl Timers {
    fn new() -> Self {
        let interval = |period| {
            let mut i = tokio::time::interval(period);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        };
        Timers {
            bitrate: interval(BITRATE_TICK),
            ping: interval(PING_INTERVAL),
            stats: interval(STATS_INTERVAL),
            capture: interval(CAPTURE_CHECK_INTERVAL),
        }
    }

    /// Streaming (re)started: ping at once (the window needs an RTT), the rest one period later.
    fn restart(&mut self) {
        self.bitrate.reset();
        self.ping.reset_immediately();
        self.stats.reset();
        self.capture.reset();
    }
}

/// `keyframe_req` handling. A request that a keyframe already on its way answers (see
/// [`rd_flow::FlowGate::keyframe_on_its_way`]) is dropped: the viewer reset its decoder before
/// asking, so any keyframe it receives after asking will do. That is what makes resume cost one
/// IDR, not two. Otherwise at most one keyframe is forced per [`KEYFRAME_MIN_INTERVAL`], and
/// requests inside the interval are coalesced into one at its end.
#[derive(Debug, Default)]
struct KeyframeLimiter {
    last: Option<Instant>,
    pending: bool,
}

impl KeyframeLimiter {
    /// True if a keyframe must be forced now; otherwise it was dropped (`on_its_way`) or
    /// deferred.
    fn request(&mut self, now: Instant, on_its_way: bool) -> bool {
        if on_its_way {
            // Answers this request and any deferred one.
            self.pending = false;
            return false;
        }
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_MIN_INTERVAL)
        {
            self.pending = true;
            return false;
        }
        self.last = Some(now);
        self.pending = false;
        true
    }

    /// True when a deferred request is due (and no keyframe is on its way by now).
    fn poll(&mut self, now: Instant, on_its_way: bool) -> bool {
        self.pending && self.request(now, on_its_way)
    }
}

/// The input thread's report of dropped input while the host is locked. If the shared state
/// did not know about the lock yet, refreshing it tells the gateway and (through the session's
/// state subscription) the viewer; otherwise the viewer gets a `state` with `locked: true`
/// directly. The input thread calls this at most once per locked period.
fn locked_reporter(ctx: Arc<Ctx>, out: Outbox) -> OnLockedDrop {
    Box::new(move || {
        if !ctx.refresh_state() {
            let mut state = *ctx.state.borrow();
            state.locked = true;
            out.json(&AgentMsg::State(state));
        }
    })
}

/// Highest version both sides speak, if their ranges overlap.
fn negotiate(viewer_max: u16, viewer_min: u16) -> Option<u16> {
    let version = viewer_max.min(PROTO_VERSION);
    (version >= viewer_min.max(MIN_PROTO_VERSION)).then_some(version)
}

fn effective(ctx: &Ctx, p: &StreamParams) -> Result<Effective, String> {
    Effective::new(p, ctx.platform.native_size(p.display), &ctx.cfg.defaults)
}

fn apply_patch(p: &mut StreamParams, patch: &StreamParamsPatch) {
    // Destructured so that a new patch field cannot be forgotten here.
    let StreamParamsPatch {
        width,
        height,
        fps,
        codec,
        start_kbps,
        min_kbps,
        max_kbps,
        adaptive,
        audio,
        display,
    } = *patch;
    p.width = width.unwrap_or(p.width);
    p.height = height.unwrap_or(p.height);
    p.fps = fps.unwrap_or(p.fps);
    p.codec = codec.unwrap_or(p.codec);
    p.start_kbps = start_kbps.unwrap_or(p.start_kbps);
    p.min_kbps = min_kbps.unwrap_or(p.min_kbps);
    p.max_kbps = max_kbps.unwrap_or(p.max_kbps);
    p.adaptive = adaptive.unwrap_or(p.adaptive);
    p.audio = audio.unwrap_or(p.audio);
    p.display = display.unwrap_or(p.display);
}

async fn spawn_input(
    platform: Arc<dyn Platform>,
    on_locked_drop: OnLockedDrop,
) -> anyhow::Result<InputThread> {
    let injector_platform = platform.clone();
    let injector = tokio::task::spawn_blocking(move || injector_platform.input_injector())
        .await
        .context("creating the input injector panicked")??;
    InputThread::spawn(injector, platform, on_locked_drop).context("spawning the input thread")
}

// ---------------------------------------------------------------------------
// socket tasks

/// Why the reader stopped.
#[derive(Debug)]
enum ReadEnd {
    Closed,
    Failed(String),
    Idle,
}

struct Reader {
    shared: Arc<Shared>,
    out: Outbox,
    input: Option<crossbeam_channel::Sender<InputEvent>>,
    viewer: mpsc::Sender<FromViewer>,
    idle: Duration,
}

async fn read_loop(mut stream: SplitStream<WsStream>, reader: Reader) -> ReadEnd {
    loop {
        let msg = match timeout(reader.idle, stream.next()).await {
            Err(_) => return ReadEnd::Idle,
            Ok(None) => return ReadEnd::Closed,
            Ok(Some(Err(e))) => return ReadEnd::Failed(e.to_string()),
            Ok(Some(Ok(msg))) => msg,
        };
        match msg {
            Message::Binary(data) => reader.on_binary(&data),
            Message::Text(text) => reader.on_text(text.as_str()),
            Message::Close(frame) => {
                debug!(?frame, "session socket closed by the other side");
                return ReadEnd::Closed;
            }
            // WebSocket pings are answered by tungstenite.
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

impl Reader {
    fn closing(&self) -> bool {
        self.shared.closing.load(Ordering::SeqCst)
    }

    fn on_binary(&self, data: &[u8]) {
        if self.closing() {
            return;
        }
        match bin::parse(data) {
            Ok(BinMsg::VideoAck(ack)) => {
                if let Some(flow) = self.shared.flow.get() {
                    flow.on_ack(&ack);
                }
            }
            Ok(BinMsg::Ping(ping)) => self.out.binary(ping.pong(now_us()).to_message()),
            Ok(BinMsg::Pong(pong)) => {
                let now = now_us();
                if let Some(flow) = self.shared.flow.get()
                    && let Some(rtt) = now.checked_sub(pong.t_origin_us)
                    && rtt < MAX_RTT_US
                {
                    flow.gate.on_rtt_sample(rtt, now);
                }
            }
            Ok(BinMsg::Input(ev)) => {
                if let Some(input) = &self.input {
                    // Fails only once the input thread is gone (session teardown).
                    let _ = input.send(ev);
                }
            }
            Ok(BinMsg::Video(..) | BinMsg::Audio(..) | BinMsg::ProbeUp(_) | BinMsg::Unknown(_)) => {
            }
            Err(e) => debug!("malformed binary message from the viewer: {e}"),
        }
    }

    fn on_text(&self, text: &str) {
        if self.closing() {
            return;
        }
        let msg = match serde_json::from_str::<ViewerMsg>(text) {
            // Newer viewers may send message types we do not know: not an error.
            Ok(ViewerMsg::Unknown) => {
                debug!("unknown viewer message ignored");
                return;
            }
            Ok(msg) => FromViewer::Msg(msg),
            Err(e) => {
                let t = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|v| Some(v.get("t")?.as_str()?.to_owned()));
                debug!(t, "unparseable viewer message: {e}");
                FromViewer::Invalid {
                    t,
                    error: e.to_string(),
                }
            }
        };
        if self.viewer.try_send(msg).is_err() {
            warn!("viewer message queue full; message dropped");
        }
    }
}

async fn write_loop(
    mut sink: SplitSink<WsStream, Message>,
    mut control: mpsc::Receiver<Message>,
    mut video: mpsc::Receiver<Message>,
) {
    loop {
        let msg = tokio::select! {
            biased;
            msg = control.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            Some(msg) = video.recv() => msg,
        };
        let closing = matches!(msg, Message::Close(_));
        if let Err(e) = sink.send(msg).await {
            debug!("session socket write failed: {e}");
            return;
        }
        if closing {
            return;
        }
    }
    let _ = sink.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate(1, 1), Some(1));
        assert_eq!(negotiate(5, 1), Some(PROTO_VERSION));
        assert_eq!(negotiate(3, 2), None);
        assert_eq!(negotiate(0, 0), None);
        assert_eq!(negotiate(1, 2), None);
    }

    #[test]
    fn keyframe_limiter() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::default();
        assert!(k.request(t0, false));
        // The keyframe forced at t0 is still on its way: requests are answered by it.
        assert!(!k.request(t0 + ms(10), true));
        assert!(!k.poll(t0 + ms(600), false), "dropped, not deferred");
        // It arrived; a new request inside the interval is deferred to its end.
        assert!(!k.request(t0 + ms(100), false));
        assert!(!k.request(t0 + ms(200), false));
        assert!(!k.poll(t0 + ms(400), false));
        assert!(k.poll(t0 + ms(500), false));
        assert!(!k.poll(t0 + ms(1500), false));
        assert!(k.request(t0 + ms(1500), false));
        // A deferred request is dropped when another keyframe (say, after a post-encode drop)
        // is on its way by the time it is due.
        assert!(!k.request(t0 + ms(1600), false));
        assert!(!k.poll(t0 + ms(2000), true));
        assert!(!k.poll(t0 + ms(2100), false));
    }

    #[test]
    fn patches() {
        let mut p = StreamParams {
            width: 1728,
            height: 1118,
            fps: 60,
            codec: Default::default(),
            start_kbps: 8000,
            min_kbps: 1000,
            max_kbps: 20000,
            adaptive: true,
            audio: false,
            display: 0,
        };
        apply_patch(
            &mut p,
            &StreamParamsPatch {
                width: Some(1280),
                height: Some(828),
                max_kbps: Some(5000),
                ..Default::default()
            },
        );
        assert_eq!(
            (p.width, p.height, p.fps, p.max_kbps),
            (1280, 828, 60, 5000)
        );
    }

    #[test]
    fn end_codes() {
        assert_eq!(End::ProtoMismatch.close_code(), 4005);
        assert_eq!(End::Stall.close_code(), 4006);
        assert!(!End::Replaced.notify_gateway());
        assert!(!End::Gateway.notify_gateway());
        assert!(End::ViewerClosed.notify_gateway());
    }
}
