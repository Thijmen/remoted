//! Network probe: `/ws/probe` (synthetic video down, PROBE_UP up, idle survival, RTT via
//! PING/PONG) and the HTTP fallbacks `/api/probe/{down,up,report}`.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rand::RngCore;
use rd_flow::{BitrateConfig, BitrateController, FlowConfig, FlowGate};
use rd_proto::bin::{self, BinMsg, Ping, VIDEO_HEADER_LEN, VideoHeader};
use rd_proto::control::{ProbeCtl, ProbeEvt, ProbeTest};
use serde::Deserialize;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval, sleep_until};
use tracing::{info, warn};

use crate::auth::ViewerIdentity;
use crate::state::AppState;
use crate::util::{Clock, close_ws};

pub const MAX_KBPS: u32 = 100_000;
pub const MAX_SECONDS: u32 = 7200;
const MAX_FPS: u32 = 240;
const MAX_CHUNK_MS: u32 = 1000;
/// PROBE_UP chunks can be large at high rates.
const PROBE_MAX_MESSAGE: usize = 1 << 20;
/// Windowed down test: bitrate controller tick and RTT ping cadence (as on the agent).
const BITRATE_TICK: Duration = Duration::from_millis(250);
const RTT_PING_EVERY: Duration = Duration::from_millis(500);
/// HTTP down chunk header: server_us u64 + seq u32.
const CHUNK_HEADER_LEN: usize = 12;

/// `GET /ws/probe`.
pub async fn ws_probe(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.write_buffer_size(0)
        .max_message_size(PROBE_MAX_MESSAGE)
        .max_frame_size(PROBE_MAX_MESSAGE)
        .on_upgrade(move |socket| run_probe(st, viewer, socket))
}

/// One running test.
struct Run {
    test: ProbeTest,
    started: Instant,
    deadline: Instant,
    frames: u64,
    bytes: u64,
    window_drops: u64,
    down: Option<Down>,
}

struct Down {
    fps: u32,
    /// Payload size in fixed-rate mode.
    payload_len: usize,
    frame_tick: Interval,
    windowed: Option<Windowed>,
}

struct Windowed {
    gate: FlowGate,
    bitrate: BitrateController,
    control_tick: Interval,
    ping_tick: Interval,
    ping_id: u32,
}

enum RunEvent {
    Frame,
    BitrateTick,
    RttPing,
    Finished,
}

fn ticker(period: Duration) -> Interval {
    let mut i = interval(period);
    i.set_missed_tick_behavior(MissedTickBehavior::Skip);
    i
}

impl Run {
    fn start(ctl: &RunParams, now_us: u64) -> Result<Self, String> {
        let RunParams {
            test,
            kbps,
            fps,
            seconds,
            windowed,
        } = *ctl;
        if !(1..=MAX_SECONDS).contains(&seconds) {
            return Err(format!("seconds must be 1..={MAX_SECONDS}"));
        }
        if kbps > MAX_KBPS {
            return Err(format!("kbps must be <= {MAX_KBPS}"));
        }
        let down = if test == ProbeTest::Down {
            if kbps == 0 {
                return Err("kbps must be > 0 for the down test".into());
            }
            if !(1..=MAX_FPS).contains(&fps) {
                return Err(format!("fps must be 1..={MAX_FPS}"));
            }
            let max_bps = kbps * 1000;
            let windowed = windowed.then(|| Windowed {
                gate: FlowGate::new(FlowConfig {
                    fps,
                    target_bps: max_bps,
                    // Synthetic frames: no encoder latency in the window horizon.
                    initial_encode_us: 0,
                    ..FlowConfig::default()
                }),
                bitrate: BitrateController::new(
                    BitrateConfig {
                        start_bps: max_bps,
                        min_bps: (max_bps / 20).max(100_000).min(max_bps),
                        max_bps,
                        adaptive: true,
                    },
                    now_us,
                ),
                control_tick: ticker(BITRATE_TICK),
                ping_tick: ticker(RTT_PING_EVERY),
                ping_id: 0,
            });
            Some(Down {
                fps,
                payload_len: (u64::from(max_bps) / 8 / u64::from(fps)) as usize,
                frame_tick: ticker(Duration::from_secs(1) / fps),
                windowed,
            })
        } else {
            None
        };
        let started = Instant::now();
        Ok(Run {
            test,
            started,
            deadline: started + Duration::from_secs(u64::from(seconds)),
            frames: 0,
            bytes: 0,
            window_drops: 0,
            down,
        })
    }

    async fn next_event(&mut self) -> RunEvent {
        let deadline = self.deadline;
        let Some(down) = self.down.as_mut() else {
            sleep_until(deadline).await;
            return RunEvent::Finished;
        };
        match down.windowed.as_mut() {
            None => tokio::select! {
                biased;
                () = sleep_until(deadline) => RunEvent::Finished,
                _ = down.frame_tick.tick() => RunEvent::Frame,
            },
            Some(w) => tokio::select! {
                biased;
                () = sleep_until(deadline) => RunEvent::Finished,
                _ = w.control_tick.tick() => RunEvent::BitrateTick,
                _ = w.ping_tick.tick() => RunEvent::RttPing,
                _ = down.frame_tick.tick() => RunEvent::Frame,
            },
        }
    }

    fn windowed(&mut self) -> Option<&mut Windowed> {
        self.down.as_mut()?.windowed.as_mut()
    }

    fn done(&self, stopped: bool) -> ProbeEvt {
        ProbeEvt::Done {
            test: self.test,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            frames: self.frames,
            bytes: self.bytes,
            window_drops: self.window_drops,
            stopped,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RunParams {
    test: ProbeTest,
    kbps: u32,
    fps: u32,
    seconds: u32,
    windowed: bool,
}

async fn next_event(run: &mut Option<Run>) -> RunEvent {
    match run {
        Some(run) => run.next_event().await,
        None => std::future::pending().await,
    }
}

/// A synthetic VIDEO frame with a random (incompressible) payload.
fn video_frame(seq: u32, capture_us: u64, payload_len: usize) -> Vec<u8> {
    let header = VideoHeader {
        flags: 0,
        epoch: 0,
        seq,
        capture_us,
        encode_us: 0,
    };
    let mut msg = Vec::with_capacity(VIDEO_HEADER_LEN + payload_len);
    header.write(&mut msg);
    msg.resize(VIDEO_HEADER_LEN + payload_len, 0);
    rand::rng().fill_bytes(&mut msg[VIDEO_HEADER_LEN..]);
    msg
}

async fn send_evt(socket: &mut WebSocket, evt: &ProbeEvt) -> bool {
    let text = serde_json::to_string(evt).expect("probe event serializes");
    socket.send(Message::Text(text.into())).await.is_ok()
}

fn bad_request(msg: impl Into<String>) -> ProbeEvt {
    ProbeEvt::Error {
        code: "bad_request".into(),
        msg: msg.into(),
    }
}

async fn run_probe(st: Arc<AppState>, viewer: ViewerIdentity, mut socket: WebSocket) {
    let _task = st.track_task();
    let t = st.cfg.timeouts;
    let clock = st.clock;
    let hello = ProbeEvt::Hello {
        server_time_us: clock.now_us(),
        client_ip: viewer.ip.to_string(),
        user: viewer.user.clone(),
    };
    if !send_evt(&mut socket, &hello).await {
        return;
    }
    info!(user = viewer.user, ip = %viewer.ip, "probe connected");

    let mut run: Option<Run> = None;
    let mut ws_ping = tokio::time::interval_at(Instant::now() + t.ping_interval, t.ping_interval);
    ws_ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_pong = Instant::now();
    let mut shutdown = std::pin::pin!(st.shutdown_signal());

    let close = loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break Some((1012, "gateway restarting")),
            msg = socket.recv() => {
                let ok = match msg {
                    Some(Ok(Message::Binary(b))) => on_binary(&mut socket, &mut run, &clock, &b).await,
                    Some(Ok(Message::Text(text))) => on_control(&st, &viewer, &mut socket, &mut run, &text).await,
                    Some(Ok(Message::Pong(_))) => {
                        last_pong = Instant::now();
                        true
                    }
                    Some(Ok(Message::Ping(_))) => true,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break None,
                };
                if !ok {
                    break None;
                }
            }
            ev = next_event(&mut run) => {
                if !on_event(&mut socket, &mut run, &clock, ev).await {
                    break None;
                }
            }
            _ = ws_ping.tick() => {
                if last_pong.elapsed() > t.pong_timeout {
                    break Some((1001, "ping timeout"));
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break None;
                }
            }
        }
    };
    if let Some(run) = &run {
        info!(user = viewer.user, ip = %viewer.ip, test = ?run.test, frames = run.frames, bytes = run.bytes, "probe run aborted by disconnect");
    }
    if let Some((code, reason)) = close {
        close_ws(socket, code, reason).await;
    }
    info!(user = viewer.user, ip = %viewer.ip, "probe disconnected");
}

/// Returns false when the socket failed.
async fn on_binary(socket: &mut WebSocket, run: &mut Option<Run>, clock: &Clock, b: &[u8]) -> bool {
    match bin::parse(b) {
        Ok(BinMsg::Ping(ping)) => {
            let pong = ping.pong(clock.now_us()).to_message();
            return socket.send(Message::Binary(pong.into())).await.is_ok();
        }
        Ok(BinMsg::Pong(pong)) => {
            if let Some(w) = run.as_mut().and_then(Run::windowed) {
                let now = clock.now_us();
                w.gate
                    .on_rtt_sample(now.saturating_sub(pong.t_origin_us), now);
            }
        }
        Ok(BinMsg::VideoAck(ack)) => {
            if let Some(w) = run.as_mut().and_then(Run::windowed) {
                let now = clock.now_us();
                let samples = w.gate.on_ack(ack.seq, u32::from(ack.decode_us), now);
                w.bitrate.on_ack_samples(&samples, now);
            }
        }
        Ok(BinMsg::ProbeUp(_)) => {
            if let Some(run) = run.as_mut().filter(|r| r.test == ProbeTest::Up) {
                run.frames += 1;
                run.bytes += b.len() as u64;
            }
        }
        Ok(_) | Err(_) => {}
    }
    true
}

async fn on_control(
    st: &AppState,
    viewer: &ViewerIdentity,
    socket: &mut WebSocket,
    run: &mut Option<Run>,
    text: &str,
) -> bool {
    match serde_json::from_str::<ProbeCtl>(text) {
        Ok(ProbeCtl::Run {
            test,
            kbps,
            fps,
            seconds,
            windowed,
        }) => {
            if run.is_some() {
                let busy = ProbeEvt::Error {
                    code: "busy".into(),
                    msg: "a test is already running".into(),
                };
                return send_evt(socket, &busy).await;
            }
            let params = RunParams {
                test,
                kbps,
                fps,
                seconds,
                windowed,
            };
            match Run::start(&params, st.clock.now_us()) {
                Ok(started) => {
                    st.metrics.probe_runs.inc();
                    info!(user = viewer.user, ip = %viewer.ip, ?test, kbps, fps, seconds, windowed, "probe run");
                    *run = Some(started);
                    true
                }
                Err(msg) => send_evt(socket, &bad_request(msg)).await,
            }
        }
        Ok(ProbeCtl::Stop) => match run.take() {
            Some(stopped) => send_evt(socket, &stopped.done(true)).await,
            None => true,
        },
        Ok(ProbeCtl::Unknown) => true,
        Err(err) => send_evt(socket, &bad_request(err.to_string())).await,
    }
}

async fn on_event(
    socket: &mut WebSocket,
    run_slot: &mut Option<Run>,
    clock: &Clock,
    ev: RunEvent,
) -> bool {
    let Some(run) = run_slot.as_mut() else {
        return true;
    };
    match ev {
        RunEvent::Finished => {
            let done = run.done(false);
            *run_slot = None;
            send_evt(socket, &done).await
        }
        RunEvent::Frame => {
            let Some(down) = run.down.as_mut() else {
                return true;
            };
            let now = clock.now_us();
            let (seq, payload_len) = match down.windowed.as_mut() {
                None => ((run.frames + 1) as u32, down.payload_len),
                Some(w) => match w.gate.admit(now) {
                    None => {
                        w.bitrate.on_captured(true, now);
                        run.window_drops += 1;
                        return true;
                    }
                    Some(admitted) => {
                        w.bitrate.on_captured(false, now);
                        let len =
                            (u64::from(w.bitrate.target_bps()) / 8 / u64::from(down.fps)) as usize;
                        (admitted.seq, len)
                    }
                },
            };
            let msg = video_frame(seq, now, payload_len);
            if let Some(w) = down.windowed.as_mut() {
                w.bitrate.on_encoded(msg.len(), now);
                w.gate.on_sent(seq, msg.len(), false, now);
            }
            run.frames += 1;
            run.bytes += msg.len() as u64;
            socket.send(Message::Binary(msg.into())).await.is_ok()
        }
        RunEvent::BitrateTick => {
            if let Some(w) = run.windowed() {
                let now = clock.now_us();
                if let Some(bps) = w.bitrate.tick(now, w.gate.stats().rtt_min_us) {
                    w.gate.set_target_bps(bps);
                }
            }
            true
        }
        RunEvent::RttPing => {
            let Some(w) = run.windowed() else {
                return true;
            };
            w.ping_id = w.ping_id.wrapping_add(1);
            let ping = Ping {
                id: w.ping_id,
                t_origin_us: clock.now_us(),
            };
            socket
                .send(Message::Binary(ping.to_message().into()))
                .await
                .is_ok()
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP probe

#[derive(Debug, Deserialize)]
pub struct DownQuery {
    #[serde(default = "default_down_kbps")]
    kbps: u32,
    #[serde(default = "default_down_seconds")]
    seconds: u32,
    #[serde(default = "default_chunk_ms")]
    chunk_ms: u32,
}

fn default_down_kbps() -> u32 {
    10_000
}
fn default_down_seconds() -> u32 {
    10
}
fn default_chunk_ms() -> u32 {
    20
}

/// `GET /api/probe/down`: a paced stream of `[server_us u64][seq u32][random filler]` chunks of
/// `kbps·chunk_ms/8` bytes. Hyper only pulls the next chunk when the previous one was written,
/// so a slow client slows the stream instead of buffering it.
pub async fn http_down(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Query(q): Query<DownQuery>,
) -> Response {
    if !(1..=MAX_KBPS).contains(&q.kbps)
        || !(1..=MAX_SECONDS).contains(&q.seconds)
        || !(1..=MAX_CHUNK_MS).contains(&q.chunk_ms)
    {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "need 1<=kbps<={MAX_KBPS}, 1<=seconds<={MAX_SECONDS}, 1<=chunk_ms<={MAX_CHUNK_MS}\n"
            ),
        )
            .into_response();
    }
    info!(user = viewer.user, ip = %viewer.ip, kbps = q.kbps, seconds = q.seconds, chunk_ms = q.chunk_ms, "probe http down");
    let chunk_len =
        ((u64::from(q.kbps) * u64::from(q.chunk_ms) / 8) as usize).max(CHUNK_HEADER_LEN);
    let state = DownStream {
        tick: ticker(Duration::from_millis(u64::from(q.chunk_ms))),
        end: Instant::now() + Duration::from_secs(u64::from(q.seconds)),
        seq: 0,
        chunk_len,
        clock: st.clock,
        shutdown: Box::pin(st.shutdown_signal()),
    };
    let stream = futures_util::stream::unfold(state, |mut s| async move {
        tokio::select! {
            biased;
            () = &mut s.shutdown => return None,
            () = sleep_until(s.end) => return None,
            _ = s.tick.tick() => {}
        }
        s.seq = s.seq.wrapping_add(1);
        let chunk = down_chunk(s.clock.now_us(), s.seq, s.chunk_len);
        Some((Ok::<_, Infallible>(Bytes::from(chunk)), s))
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

struct DownStream {
    tick: Interval,
    end: Instant,
    seq: u32,
    chunk_len: usize,
    clock: Clock,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
}

fn down_chunk(server_us: u64, seq: u32, len: usize) -> Vec<u8> {
    let mut chunk = vec![0u8; len];
    chunk[..8].copy_from_slice(&server_us.to_le_bytes());
    chunk[8..CHUNK_HEADER_LEN].copy_from_slice(&seq.to_le_bytes());
    rand::rng().fill_bytes(&mut chunk[CHUNK_HEADER_LEN..]);
    chunk
}

/// `POST /api/probe/up`: count a streamed request body.
pub async fn http_up(Extension(viewer): Extension<ViewerIdentity>, body: Body) -> Response {
    let started = Instant::now();
    let mut bytes = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => bytes += chunk.len() as u64,
            Err(err) => {
                warn!(user = viewer.user, ip = %viewer.ip, error = %err, bytes, "probe http up: body error");
                return (StatusCode::BAD_REQUEST, "body error\n").into_response();
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    info!(user = viewer.user, ip = %viewer.ip, bytes, elapsed_ms, "probe http up");
    Json(serde_json::json!({ "bytes": bytes, "elapsed_ms": elapsed_ms })).into_response()
}

/// `POST /api/probe/report`: log the JSON report as one line.
pub async fn http_report(Extension(viewer): Extension<ViewerIdentity>, body: Bytes) -> Response {
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(report) => {
            info!(target: "probe_report", user = viewer.user, ip = %viewer.ip, "probe_report {report}");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => (StatusCode::BAD_REQUEST, format!("invalid JSON: {err}\n")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_chunks() {
        let f = video_frame(7, 99, 100);
        assert_eq!(f.len(), VIDEO_HEADER_LEN + 100);
        match bin::parse(&f).unwrap() {
            BinMsg::Video(h, p) => {
                assert_eq!((h.seq, h.capture_us, h.epoch, h.flags), (7, 99, 0, 0));
                assert_eq!(p.len(), 100);
            }
            other => panic!("{other:?}"),
        }
        let c = down_chunk(5, 6, 25_000);
        assert_eq!(c.len(), 25_000);
        assert_eq!(u64::from_le_bytes(c[..8].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(c[8..12].try_into().unwrap()), 6);
    }

    #[tokio::test]
    async fn run_limits() {
        let p = |test, kbps, fps, seconds| RunParams {
            test,
            kbps,
            fps,
            seconds,
            windowed: false,
        };
        assert!(Run::start(&p(ProbeTest::Down, 10_000, 60, 20), 0).is_ok());
        assert!(Run::start(&p(ProbeTest::Down, MAX_KBPS + 1, 60, 20), 0).is_err());
        assert!(Run::start(&p(ProbeTest::Down, 0, 60, 20), 0).is_err());
        assert!(Run::start(&p(ProbeTest::Down, 1000, 0, 20), 0).is_err());
        assert!(Run::start(&p(ProbeTest::Idle, 0, 60, MAX_SECONDS + 1), 0).is_err());
        assert!(Run::start(&p(ProbeTest::Idle, 0, 60, 0), 0).is_err());
        assert!(Run::start(&p(ProbeTest::Up, 1000, 60, MAX_SECONDS), 0).is_ok());
    }
}
