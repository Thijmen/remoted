//! Test doubles: a [`FakePlatform`] (synthetic encoder, recording input injector, scripted lock
//! state), an in-process WebSocket server playing the gateway, and a scripted viewer.

#![allow(dead_code)] // each test binary uses a different subset

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use rd_agent_core::clock::now_us;
use rd_agent_core::config::AgentConfig;
use rd_agent_core::runtime::{Runtime, Timings};
use rd_agent_core::{
    CodecConfig, DisplayDesc, EncodedFrame, FrameGate, FrameSink, Guard, InputEvent, InputInjector,
    LockState, Perms, Platform, VideoConfig, VideoPipeline,
};
use rd_proto::bin::{self, BinMsg, VideoAck, VideoHeader};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

pub const TOKEN: &str = "rd1.mbp.secret";
pub const SESSION_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const SESSION_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Every wait in the tests is bounded by this.
pub const WAIT: Duration = Duration::from_secs(5);
/// Overall limit for `expect`, which skips unrelated messages (video keeps arriving).
pub const EXPECT_WAIT: Duration = Duration::from_secs(15);
/// Fake avcC: version 1, profile 0x64, compat 0, level 0x33.
pub const AVCC: &[u8] = &[1, 0x64, 0, 0x33, 0xff, 0xe1, 0, 0];

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rd_agent_core=debug".into()),
        )
        .try_init();
}

// ---------------------------------------------------------------------------
// platform

/// What the fake platform saw, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PipelineStart(VideoConfig),
    PipelineReconfigure(VideoConfig),
    PipelineStop,
    Bitrate(u32),
    Input(InputEvent),
    ReleaseAll,
    AwakeAcquired,
    AwakeReleased,
    UserActivity,
}

pub struct FakePlatform {
    locked: AtomicBool,
    perms: Mutex<Perms>,
    events: Arc<Mutex<Vec<Event>>>,
    /// Capture rate of the fake pipeline, independent of the requested fps.
    capture_fps: u32,
    /// What `VideoPipeline::failure` reports.
    capture_failure: Arc<Mutex<Option<String>>>,
    /// When set, `VideoPipeline::start` fails with this message.
    start_failure: Arc<Mutex<Option<String>>>,
    /// Nothing changes on screen: after its first frame the pipeline only captures when the
    /// gate wants a refresh (like ScreenCaptureKit, which delivers no frame for an idle screen).
    static_screen: Arc<AtomicBool>,
}

impl FakePlatform {
    pub fn new(capture_fps: u32) -> Arc<Self> {
        Arc::new(FakePlatform {
            locked: AtomicBool::new(false),
            perms: Mutex::new(Perms {
                screen: true,
                input: true,
            }),
            events: Arc::default(),
            capture_fps,
            capture_failure: Arc::default(),
            start_failure: Arc::default(),
            static_screen: Arc::default(),
        })
    }

    pub fn set_capture_failure(&self, reason: Option<&str>) {
        *self.capture_failure.lock() = reason.map(str::to_owned);
    }

    pub fn set_start_failure(&self, reason: Option<&str>) {
        *self.start_failure.lock() = reason.map(str::to_owned);
    }

    pub fn set_static_screen(&self, on: bool) {
        self.static_screen.store(on, Ordering::SeqCst);
    }

    pub fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::SeqCst);
    }

    pub fn set_perms(&self, perms: Perms) {
        *self.perms.lock() = perms;
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().clone()
    }

    pub fn count(&self, pred: impl Fn(&Event) -> bool) -> usize {
        self.events.lock().iter().filter(|e| pred(e)).count()
    }

    /// Waits until `pred` holds for the recorded events.
    pub async fn wait_for(&self, what: &str, pred: impl Fn(&[Event]) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !pred(&self.events.lock()) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events: {:?}",
                self.events()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn record(&self, e: Event) {
        self.events.lock().push(e);
    }
}

impl Platform for FakePlatform {
    fn os(&self) -> &'static str {
        "fakeos"
    }
    fn os_description(&self) -> String {
        "FakeOS 1.0".into()
    }
    fn displays(&self) -> Vec<DisplayDesc> {
        vec![DisplayDesc {
            id: 0,
            w_pt: 1728,
            h_pt: 1117,
            scale: 2.0,
        }]
    }
    fn native_size(&self, display: u32) -> Option<(u32, u32)> {
        (display == 0).then_some((3456, 2234))
    }
    fn permissions(&self) -> Perms {
        *self.perms.lock()
    }
    fn lock_state(&self) -> LockState {
        if self.locked.load(Ordering::SeqCst) {
            LockState::Locked
        } else {
            LockState::Unlocked
        }
    }
    fn keep_awake(&self, _reason: &str) -> Box<dyn Guard> {
        self.record(Event::AwakeAcquired);
        Box::new(AwakeGuard(self.events.clone()))
    }
    fn declare_user_activity(&self) {
        self.record(Event::UserActivity);
    }
    fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>> {
        Ok(Box::new(FakePipeline {
            events: self.events.clone(),
            capture_fps: self.capture_fps,
            failure: self.capture_failure.clone(),
            start_failure: self.start_failure.clone(),
            static_screen: self.static_screen.clone(),
            running: None,
            gate: None,
            sink: None,
        }))
    }
    fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(RecordingInjector(self.events.clone())))
    }
}

struct AwakeGuard(Arc<Mutex<Vec<Event>>>);

impl Drop for AwakeGuard {
    fn drop(&mut self) {
        self.0.lock().push(Event::AwakeReleased);
    }
}

struct RecordingInjector(Arc<Mutex<Vec<Event>>>);

impl InputInjector for RecordingInjector {
    fn inject(&mut self, ev: InputEvent) {
        self.0.lock().push(Event::Input(ev));
    }
    fn release_all(&mut self) {
        self.0.lock().push(Event::ReleaseAll);
    }
}

/// "Captures" at `capture_fps` through the gate and emits small fake AVCC frames. The first
/// frame after start/reconfigure is an IDR with a [`CodecConfig`] for the configured size.
struct FakePipeline {
    events: Arc<Mutex<Vec<Event>>>,
    capture_fps: u32,
    failure: Arc<Mutex<Option<String>>>,
    start_failure: Arc<Mutex<Option<String>>>,
    static_screen: Arc<AtomicBool>,
    running: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    gate: Option<Arc<dyn FrameGate>>,
    sink: Option<FrameSink>,
}

impl FakePipeline {
    fn spawn(&mut self, cfg: VideoConfig) {
        let (Some(gate), Some(sink)) = (self.gate.clone(), self.sink.clone()) else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let static_screen = self.static_screen.clone();
        let period = Duration::from_secs(1) / self.capture_fps;
        let thread = std::thread::spawn(move || {
            let mut need_config = true;
            while !thread_stop.load(Ordering::SeqCst) {
                std::thread::sleep(period);
                if !need_config && static_screen.load(Ordering::SeqCst) && !gate.refresh_wanted() {
                    continue;
                }
                let capture_us = now_us();
                let Some(admitted) = gate.admit(capture_us) else {
                    continue;
                };
                let key = need_config || admitted.force_keyframe;
                let frame = EncodedFrame {
                    seq: admitted.seq,
                    capture_us,
                    encode_us: 1500,
                    key,
                    data: fake_avcc_frame(key),
                    config: need_config.then(|| CodecConfig {
                        codec: "avc1.640033".into(),
                        width: cfg.width,
                        height: cfg.height,
                        avcc: Bytes::from_static(AVCC),
                    }),
                };
                need_config = false;
                if sink.try_send(frame).is_err() {
                    gate.post_encode_drop(admitted.seq);
                }
            }
        });
        self.running = Some((stop, thread));
    }

    fn halt(&mut self) {
        if let Some((stop, thread)) = self.running.take() {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
        }
    }
}

fn fake_avcc_frame(key: bool) -> Bytes {
    let len = if key { 6000 } else { 1500 };
    let mut v = Vec::with_capacity(4 + len);
    v.extend_from_slice(&(len as u32).to_be_bytes());
    v.push(if key { 0x65 } else { 0x41 });
    v.resize(4 + len, 0xab);
    Bytes::from(v)
}

impl VideoPipeline for FakePipeline {
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<()> {
        self.halt();
        if let Some(reason) = self.start_failure.lock().clone() {
            anyhow::bail!("{reason}");
        }
        self.events.lock().push(Event::PipelineStart(cfg));
        self.gate = Some(gate);
        self.sink = Some(sink);
        self.spawn(cfg);
        Ok(())
    }
    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        self.halt();
        self.events.lock().push(Event::PipelineReconfigure(cfg));
        self.spawn(cfg);
        Ok(())
    }
    fn set_bitrate(&self, bps: u32) {
        self.events.lock().push(Event::Bitrate(bps));
    }
    fn failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }
    fn stop(&mut self) {
        let was_running = self.running.is_some();
        self.halt();
        self.gate = None;
        self.sink = None;
        if was_running {
            self.events.lock().push(Event::PipelineStop);
        }
    }
}

impl Drop for FakePipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// agent

pub fn timings() -> Timings {
    Timings {
        state_poll: Duration::from_millis(50),
        backoff_min: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        ..Timings::default()
    }
}

pub struct Agent {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Agent {
    pub fn start(gateway_url: &str, platform: Arc<FakePlatform>, timings: Timings) -> Agent {
        Agent::start_with(gateway_url, platform, timings, |runtime| runtime)
    }

    /// Like [`Agent::start`], with `configure` applied to the runtime (e.g. an update hookup).
    pub fn start_with(
        gateway_url: &str,
        platform: Arc<FakePlatform>,
        timings: Timings,
        configure: impl FnOnce(Runtime) -> Runtime,
    ) -> Agent {
        let cfg = AgentConfig {
            gateway_url: gateway_url.into(),
            machine: "mbp".into(),
            token: Some(TOKEN.into()),
            defaults: Default::default(),
            update: Default::default(),
        };
        let (tx, rx) = oneshot::channel();
        let runtime = configure(Runtime::new(cfg, platform).timings(timings));
        let task = tokio::spawn(runtime.run_until(async {
            let _ = rx.await;
        }));
        Agent {
            shutdown: Some(tx),
            task,
        }
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let result = tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("agent did not shut down")
            .expect("agent task panicked");
        result.expect("agent returned an error");
    }
}

// ---------------------------------------------------------------------------
// gateway / viewer sockets

pub type Ws = WebSocketStream<TcpStream>;

/// One accepted WebSocket with its request line and Authorization header.
pub struct Conn {
    pub path: String,
    pub auth: Option<String>,
    pub ws: Ws,
}

/// A message received by the test side.
#[derive(Debug)]
pub enum Msg {
    Json(Value),
    Bin(Vec<u8>),
    Close(Option<u16>),
}

impl Msg {
    pub fn t(&self) -> Option<&str> {
        match self {
            Msg::Json(v) => v["t"].as_str(),
            _ => None,
        }
    }

    pub fn video(&self) -> Option<VideoHeader> {
        match self {
            Msg::Bin(b) => match bin::parse(b) {
                Ok(BinMsg::Video(h, _)) => Some(h),
                _ => None,
            },
            _ => None,
        }
    }
}

pub async fn recv(ws: &mut Ws) -> Msg {
    loop {
        let msg = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("timed out waiting for a message");
        match msg {
            None => return Msg::Close(None),
            Some(Err(_)) => return Msg::Close(None),
            Some(Ok(Message::Text(t))) => {
                return Msg::Json(serde_json::from_str(t.as_str()).expect("invalid JSON"));
            }
            Some(Ok(Message::Binary(b))) => return Msg::Bin(b.to_vec()),
            Some(Ok(Message::Close(f))) => return Msg::Close(f.map(|f| u16::from(f.code))),
            Some(Ok(_)) => {}
        }
    }
}

pub async fn send_json(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into()))
        .await
        .expect("send failed");
}

pub async fn send_bin(ws: &mut Ws, data: Vec<u8>) {
    ws.send(Message::Binary(data.into()))
        .await
        .expect("send failed");
}

/// Plays the gateway: accepts the agent's control and session sockets.
pub struct FakeGateway {
    pub url: String,
    controls: mpsc::UnboundedReceiver<Conn>,
    sessions: mpsc::UnboundedReceiver<Conn>,
    /// HTTP statuses for the next control upgrades (instead of accepting them).
    reject: Arc<Mutex<VecDeque<u16>>>,
    _accept: tokio::task::JoinHandle<()>,
}

impl FakeGateway {
    pub async fn start() -> FakeGateway {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (control_tx, controls) = mpsc::unbounded_channel();
        let (session_tx, sessions) = mpsc::unbounded_channel();
        let reject: Arc<Mutex<VecDeque<u16>>> = Arc::default();
        let reject_accept = reject.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (control_tx, session_tx) = (control_tx.clone(), session_tx.clone());
                let reject = reject_accept.clone();
                tokio::spawn(async move {
                    let seen = Arc::new(Mutex::new((String::new(), None)));
                    let seen_cb = seen.clone();
                    #[allow(clippy::result_large_err)] // the signature tungstenite expects
                    let callback = move |req: &Request, resp: Response| {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        let path = req.uri().path().to_owned();
                        if path == "/ws/agent"
                            && let Some(status) = reject.lock().pop_front()
                        {
                            let mut err = ErrorResponse::new(Some("rejected by the test".into()));
                            *err.status_mut() = StatusCode::from_u16(status).unwrap();
                            return Err(err);
                        }
                        *seen_cb.lock() = (path, auth);
                        Ok(resp)
                    };
                    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                        return;
                    };
                    let (path, auth) = seen.lock().clone();
                    let conn = Conn { path, auth, ws };
                    let tx = if conn.path == "/ws/agent" {
                        control_tx
                    } else {
                        session_tx
                    };
                    let _ = tx.send(conn);
                });
            }
        });
        FakeGateway {
            url,
            controls,
            sessions,
            reject,
            _accept: accept,
        }
    }

    /// Answer the next control upgrades with these HTTP statuses.
    pub fn reject_controls(&self, statuses: &[u16]) {
        self.reject.lock().extend(statuses);
    }

    /// The next control connection, after checking its `register` and answering it.
    pub async fn control(&mut self) -> Control {
        let mut conn = tokio::time::timeout(WAIT, self.controls.recv())
            .await
            .expect("no control connection")
            .unwrap();
        assert_eq!(conn.auth.as_deref(), Some(&*format!("Bearer {TOKEN}")));
        let register = recv(&mut conn.ws).await;
        let Msg::Json(register) = register else {
            panic!("expected register, got {register:?}");
        };
        assert_eq!(register["t"], "register");
        send_json(&mut conn.ws, json!({"t": "registered", "gw_proto": 1})).await;
        Control {
            ws: conn.ws,
            register,
        }
    }

    pub async fn session(&mut self) -> Conn {
        tokio::time::timeout(WAIT, self.sessions.recv())
            .await
            .expect("no session connection")
            .unwrap()
    }

    /// True if no new control connection arrives within `d`.
    pub async fn no_control_within(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, self.controls.recv()).await.is_err()
    }
}

pub struct Control {
    pub ws: Ws,
    pub register: Value,
}

impl Control {
    /// Next JSON message of type `t` (others are skipped).
    pub async fn expect(&mut self, t: &str) -> Value {
        let wait = async {
            loop {
                match recv(&mut self.ws).await {
                    Msg::Json(v) if v["t"] == t => return v,
                    Msg::Json(_) | Msg::Bin(_) => {}
                    Msg::Close(code) => panic!("control closed ({code:?}) while waiting for {t}"),
                }
            }
        };
        tokio::time::timeout(EXPECT_WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {t} on the control socket"))
    }

    /// Next `state` message matching `pred`.
    pub async fn expect_state(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let v = self.expect("state").await;
            if pred(&v) {
                return v;
            }
        }
    }

    pub async fn request_session(&mut self, id: &str) {
        send_json(
            &mut self.ws,
            json!({"t": "session_request", "session": id, "user": "me@test", "client_ip": "127.0.0.1"}),
        )
        .await;
    }
}

/// Plays the viewer on an accepted session socket.
pub struct Viewer {
    pub ws: Ws,
    /// ACK every VIDEO frame as soon as it arrives.
    pub auto_ack: bool,
    /// Answer agent PINGs.
    pub answer_pings: bool,
    pub pongs: Vec<rd_proto::bin::Pong>,
}

impl Viewer {
    pub fn new(ws: Ws) -> Viewer {
        Viewer {
            ws,
            auto_ack: true,
            answer_pings: true,
            pongs: Vec::new(),
        }
    }

    pub async fn hello(&mut self) -> Value {
        send_json(
            &mut self.ws,
            json!({"t": "hello", "proto": 1, "min_proto": 1, "viewer": {"ua": "test", "dpr": 2, "viewport": [1536, 750], "decoders": {"avc": true}}}),
        )
        .await;
        self.expect("hello").await
    }

    pub async fn start(&mut self, width: u32, height: u32) {
        send_json(
            &mut self.ws,
            json!({"t": "start", "width": width, "height": height, "fps": 60, "codec": "avc",
                   "start_kbps": 8000, "min_kbps": 1000, "max_kbps": 20000, "adaptive": true}),
        )
        .await;
    }

    pub async fn send(&mut self, v: Value) {
        send_json(&mut self.ws, v).await;
    }

    /// Sends `msgs` back to back in one write, like a browser sending them from one task.
    pub async fn send_burst(&mut self, msgs: impl IntoIterator<Item = Value>) {
        for v in msgs {
            self.ws
                .feed(Message::Text(v.to_string().into()))
                .await
                .expect("send failed");
        }
        self.ws.flush().await.expect("send failed");
    }

    pub async fn ack(&mut self, h: &VideoHeader) {
        let ack = VideoAck {
            flags: 0,
            epoch: h.epoch,
            seq: h.seq,
            decode_us: 800,
            queue_us: 10,
        };
        // Best effort: the agent may be closing the session right now.
        let _ = self.ws.send(Message::Binary(ack.to_message().into())).await;
    }

    /// Next message the test cares about: JSON (except `stats`), VIDEO or a close. PINGs and
    /// PONGs are handled on the way.
    pub async fn next(&mut self) -> Msg {
        loop {
            let msg = recv(&mut self.ws).await;
            match &msg {
                Msg::Json(v) if v["t"] == "stats" => continue,
                Msg::Bin(b) => match bin::parse(b) {
                    Ok(BinMsg::Ping(p)) => {
                        if self.answer_pings {
                            let pong = p.pong(1).to_message();
                            let _ = self.ws.send(Message::Binary(pong.into())).await;
                        }
                        continue;
                    }
                    Ok(BinMsg::Pong(p)) => {
                        self.pongs.push(p);
                        continue;
                    }
                    Ok(BinMsg::Video(h, _)) if self.auto_ack => self.ack(&h).await,
                    _ => {}
                },
                _ => {}
            }
            return msg;
        }
    }

    /// Skips to the next JSON message of type `t`.
    pub async fn expect(&mut self, t: &str) -> Value {
        let wait = async {
            loop {
                match self.next().await {
                    Msg::Json(v) if v["t"] == t => return v,
                    Msg::Close(code) => panic!("session closed ({code:?}) while waiting for {t}"),
                    _ => {}
                }
            }
        };
        // `next` alone never times out while video keeps arriving.
        tokio::time::timeout(EXPECT_WAIT, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {t} on the session"))
    }

    /// Skips to the next VIDEO frame.
    pub async fn next_video(&mut self) -> VideoHeader {
        let wait = async {
            loop {
                match self.next().await {
                    msg @ Msg::Bin(_) => {
                        if let Some(h) = msg.video() {
                            return h;
                        }
                    }
                    Msg::Close(code) => {
                        panic!("session closed ({code:?}) while waiting for video")
                    }
                    Msg::Json(_) => {}
                }
            }
        };
        tokio::time::timeout(EXPECT_WAIT, wait)
            .await
            .expect("timed out waiting for video")
    }

    /// Collects the JSON messages (except `stats`) received in the next `d`.
    pub async fn json_for(&mut self, d: Duration) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + d;
        let mut out = Vec::new();
        while let Ok(msg) = tokio::time::timeout_at(deadline, self.next()).await {
            match msg {
                Msg::Close(code) => panic!("session closed ({code:?})"),
                Msg::Json(v) => out.push(v),
                Msg::Bin(_) => {}
            }
        }
        out
    }

    /// Collects VIDEO headers for `d` (other messages are handled/skipped).
    pub async fn videos_for(&mut self, d: Duration) -> Vec<VideoHeader> {
        let deadline = tokio::time::Instant::now() + d;
        let mut out = Vec::new();
        while let Ok(msg) = tokio::time::timeout_at(deadline, self.next()).await {
            match msg {
                Msg::Close(code) => panic!("session closed ({code:?})"),
                msg => out.extend(msg.video()),
            }
        }
        out
    }

    /// Waits for the agent's close frame (skipping everything else).
    pub async fn expect_close(&mut self) -> Option<u16> {
        loop {
            if let Msg::Close(code) = self.next().await {
                return code;
            }
        }
    }
}

/// Gateway + registered agent + its control socket.
pub struct Setup {
    pub gw: FakeGateway,
    pub control: Control,
    pub agent: Agent,
    pub platform: Arc<FakePlatform>,
}

pub async fn setup_with(platform: Arc<FakePlatform>, timings: Timings) -> Setup {
    init_tracing();
    let mut gw = FakeGateway::start().await;
    let agent = Agent::start(&gw.url, platform.clone(), timings);
    let control = gw.control().await;
    Setup {
        gw,
        control,
        agent,
        platform,
    }
}

pub async fn setup() -> Setup {
    setup_with(FakePlatform::new(100), timings()).await
}

impl Setup {
    /// session_request + accepted data socket + hello exchange.
    pub async fn open_session(&mut self, id: &str) -> Viewer {
        self.control.request_session(id).await;
        let conn = self.gw.session().await;
        assert_eq!(conn.path, format!("/ws/agent/session/{id}"));
        assert_eq!(conn.auth.as_deref(), Some(&*format!("Bearer {TOKEN}")));
        let mut viewer = Viewer::new(conn.ws);
        viewer.hello().await;
        viewer
    }

    /// Opens a session and starts 1728x1118 video; returns once the first frame arrived.
    pub async fn streaming_session(&mut self, id: &str) -> (Viewer, VideoHeader) {
        let mut viewer = self.open_session(id).await;
        viewer.start(1728, 1118).await;
        viewer.expect("video_config").await;
        let first = viewer.next_video().await;
        (viewer, first)
    }
}
