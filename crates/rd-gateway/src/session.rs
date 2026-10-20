//! Viewer sessions: `/ws/view/{machine}` on the viewer port and the agent's per-session data
//! socket `/ws/agent/session/{session}` on the agent port, joined by a relay.
//!
//! The relay ([`relay`], also used for file channels: `crate::files`) is two pumps
//! (agent→viewer, viewer→agent) running concurrently in the viewer's task. Each pump reads one message and awaits its send before reading the next, so a slow
//! reader on either side pushes back through TCP to the sender; nothing is queued, dropped or
//! reordered in between. WebSocket Ping/Pong is per leg and never forwarded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rd_proto::control::GatewayToViewer;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until};
use tracing::{debug, info, warn};

use crate::auth::{AuthRejection, ViewerIdentity};
use crate::chaos::ChaosStream;
use crate::config::{Timeouts, valid_machine_name};
use crate::registry::{EndReason, SessionHandle};
use crate::state::AppState;
use crate::util::{client_ip, close_split, close_ws};

/// Largest viewer→agent message (input, ACKs, control JSON).
pub const VIEWER_MAX_MESSAGE: usize = 64 << 10;
/// Largest agent→viewer message (a keyframe).
pub const AGENT_MAX_MESSAGE: usize = 16 << 20;

/// `GET /ws/view/{machine}` (viewer port; auth already checked by middleware).
pub async fn ws_view(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.write_buffer_size(0)
        .max_message_size(VIEWER_MAX_MESSAGE)
        .max_frame_size(VIEWER_MAX_MESSAGE)
        .on_upgrade(move |socket| run_viewer(st, viewer, machine, socket))
}

/// `GET /ws/agent/session/{session}` (agent port).
pub async fn ws_agent_session(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(session): Path<String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let ident = match st
        .agent_auth
        .verify(&headers, ip, "/ws/agent/session")
        .await
    {
        Ok(ident) => ident,
        Err(rejection) => return agent_auth_failed(&st, rejection),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let (machine, deliver) = match st
        .registry
        .claim_pending(&session, ident.machine.as_deref())
    {
        Ok(claimed) => claimed,
        Err(err) => {
            warn!(%ip, session, machine = ident.machine.as_deref(), error = %err, "agent session socket rejected");
            return (StatusCode::NOT_FOUND, "unknown or expired session\n").into_response();
        }
    };
    debug!(%ip, machine, session, "agent session socket accepted");
    ws.write_buffer_size(0)
        .max_message_size(AGENT_MAX_MESSAGE)
        .max_frame_size(AGENT_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            if let Err(socket) = deliver.send(socket) {
                // The viewer gave up between claim and upgrade.
                close_ws(socket, 1000, "session ended").await;
            }
        })
}

pub fn agent_auth_failed(st: &AppState, rejection: AuthRejection) -> Response {
    st.metrics.agent_auth_failures.inc();
    rejection.into_response()
}

async fn run_viewer(st: Arc<AppState>, viewer: ViewerIdentity, machine: String, socket: WebSocket) {
    let _task = st.track_task();
    let t = st.cfg.timeouts;
    let ip = viewer.ip.to_string();

    let handle = if valid_machine_name(&machine) {
        st.registry
            .start_session(&machine, &viewer.user, &ip, t.session_request)
            .ok()
    } else {
        None
    };
    let Some(handle) = handle else {
        info!(
            user = viewer.user,
            ip, machine, "viewer rejected: agent offline"
        );
        let (code, reason) = EndReason::AgentOffline.viewer_close();
        close_ws(socket, code, reason).await;
        return;
    };
    st.metrics.sessions_started.inc();
    info!(target: "audit", event = "session_start", user = viewer.user, ip, machine, session = handle.id, "session_start");
    let started = Instant::now();
    let id = handle.id.clone();
    let (reason, traffic) = relay(&st, Kind::Session, &machine, handle, socket).await;
    let (up, down) = traffic.totals();
    st.metrics.bytes_up.add(up);
    st.metrics.bytes_down.add(down);
    info!(
        target: "audit",
        event = "session_end",
        user = viewer.user,
        ip,
        machine,
        session = id,
        duration_ms = started.elapsed().as_millis() as u64,
        bytes_up = up,
        bytes_down = down,
        reason = reason.as_str(),
        "session_end"
    );
}

/// What a relay joins: a screen session or a file channel. They differ in their registry
/// slot, their gauge and what counts as activity for the idle timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Session,
    Files,
}

impl Kind {
    fn activate(self, st: &AppState, machine: &str, id: &str) -> rd_proto::control::AgentState {
        match self {
            Kind::Session => st.registry.activate(machine, id),
            Kind::Files => st.registry.activate_files(machine, id),
        }
    }

    fn finish(self, st: &AppState, machine: &str, id: &str, reason: &EndReason) {
        match self {
            Kind::Session => st.registry.finish_session(machine, id, reason),
            Kind::Files => st.registry.finish_files(machine, id),
        }
    }

    fn active(self, st: &AppState) -> &crate::metrics::Gauge {
        match self {
            Kind::Session => &st.metrics.sessions_active,
            Kind::Files => &st.metrics.files_active,
        }
    }

    /// Screen sessions stay alive on viewer input; file channels on any message either way.
    fn activity(self, dir: Dir, msg: &Message) -> bool {
        match self {
            Kind::Session => {
                dir == Dir::Up
                    && matches!(msg, Message::Binary(b)
                        if b.first().is_some_and(|&ty| rd_proto::bin::ty::is_input(ty)))
            }
            Kind::Files => matches!(msg, Message::Text(_) | Message::Binary(_)),
        }
    }
}

/// Waits for the agent's data socket of `handle`, joins it with the viewer's `socket`, relays
/// until either side ends, frees the registry slot and closes both sockets.
pub(crate) async fn relay(
    st: &AppState,
    kind: Kind,
    machine: &str,
    handle: SessionHandle,
    socket: WebSocket,
) -> (EndReason, Traffic) {
    let t = st.cfg.timeouts;
    let SessionHandle {
        id,
        mut agent_socket,
        mut end,
        deadline,
    } = handle;
    let traffic = Traffic::default();
    let (mut viewer_tx, mut viewer_rx) = socket.split();
    let mut shutdown = std::pin::pin!(st.shutdown_signal());

    // Wait for the agent's data socket. A message the viewer sends early is held (at most one;
    // the viewer is not read further) and forwarded first after the join.
    let mut early: Option<Message> = None;
    let joined = loop {
        tokio::select! {
            biased;
            r = &mut end => break Err(r.unwrap_or(EndReason::Shutdown)),
            () = &mut shutdown => break Err(EndReason::Shutdown),
            r = &mut agent_socket => break r.map_err(|_| EndReason::AgentClosed(None)),
            () = sleep_until(deadline) => break Err(EndReason::AgentDialTimeout),
            m = viewer_rx.next(), if early.is_none() => match m {
                Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => early = Some(msg),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break Err(EndReason::ViewerClosed),
            },
        }
    };

    let reason = match joined {
        Err(reason) => {
            kind.finish(st, machine, &id, &reason);
            let (code, text) = reason.viewer_close();
            close_split(viewer_tx, viewer_rx, code, text).await;
            reason
        }
        Ok(agent) => {
            let agent_state = kind.activate(st, machine, &id);
            let (mut agent_tx, mut agent_rx) = agent.split();
            let hello = GatewayToViewer::Session {
                machine: machine.to_owned(),
                session: id.clone(),
                agent: agent_state,
            };
            let hello = serde_json::to_string(&hello).expect("session message serializes");
            let setup = if viewer_tx.send(Message::Text(hello.into())).await.is_err() {
                Err(EndReason::ViewerClosed)
            } else if let Some(msg) = early.take() {
                traffic.count(Dir::Up, &msg);
                agent_tx
                    .send(msg)
                    .await
                    .map_err(|_| EndReason::AgentClosed(None))
            } else {
                Ok(())
            };
            kind.active(st).inc();
            let reason = match setup {
                Err(reason) => reason,
                Ok(()) => {
                    let live = Liveness::new();
                    let mut agent_rx = ChaosStream::new(&mut agent_rx, st.cfg.chaos);
                    tokio::select! {
                        r = pump(kind, Dir::Down, &mut agent_rx, &mut viewer_tx, &live, &traffic, t) => r,
                        r = pump(kind, Dir::Up, &mut viewer_rx, &mut agent_tx, &live, &traffic, t) => r,
                        r = watchdog(&live, t) => r,
                        r = &mut end => r.unwrap_or(EndReason::Shutdown),
                        () = &mut shutdown => EndReason::Shutdown,
                    }
                }
            };
            kind.active(st).dec();
            // Free the slot and tell the agent first; the close handshakes may take a while.
            kind.finish(st, machine, &id, &reason);
            let (vcode, vtext) = reason.viewer_close();
            let (acode, atext) = reason.agent_close();
            tokio::join!(
                close_split(viewer_tx, viewer_rx, vcode, vtext),
                close_split(agent_tx, agent_rx, acode, atext),
            );
            reason
        }
    };
    (reason, traffic)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    /// agent → viewer
    Down,
    /// viewer → agent
    Up,
}

impl Dir {
    /// Reason when this direction's source leg closes (with the peer's close code, if any).
    fn src_closed(self, code: Option<u16>) -> EndReason {
        match self {
            Dir::Down => EndReason::AgentClosed(code),
            Dir::Up => EndReason::ViewerClosed,
        }
    }
    /// Reason when sending to this direction's destination leg fails.
    fn dst_closed(self) -> EndReason {
        match self {
            Dir::Down => EndReason::ViewerClosed,
            Dir::Up => EndReason::AgentClosed(None),
        }
    }
}

/// Payload byte counters per direction.
#[derive(Debug, Default)]
pub(crate) struct Traffic {
    up: AtomicU64,
    down: AtomicU64,
}

impl Traffic {
    fn count(&self, dir: Dir, msg: &Message) {
        let n = match msg {
            Message::Text(t) => t.len(),
            Message::Binary(b) => b.len(),
            _ => return,
        } as u64;
        match dir {
            Dir::Down => self.down.fetch_add(n, Ordering::Relaxed),
            Dir::Up => self.up.fetch_add(n, Ordering::Relaxed),
        };
    }
    pub(crate) fn totals(&self) -> (u64, u64) {
        (
            self.up.load(Ordering::Relaxed),
            self.down.load(Ordering::Relaxed),
        )
    }
}

/// Last Pong per leg and last activity (viewer input; any message for file channels), in ms
/// since the join.
struct Liveness {
    start: Instant,
    viewer_pong_ms: AtomicU64,
    agent_pong_ms: AtomicU64,
    input_ms: AtomicU64,
}

impl Liveness {
    fn new() -> Self {
        Liveness {
            start: Instant::now(),
            viewer_pong_ms: AtomicU64::new(0),
            agent_pong_ms: AtomicU64::new(0),
            input_ms: AtomicU64::new(0),
        }
    }
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn mark(&self, field: &AtomicU64) {
        field.store(self.now_ms(), Ordering::Relaxed);
    }
    fn pong(&self, from: Dir) {
        match from {
            Dir::Down => self.mark(&self.agent_pong_ms),
            Dir::Up => self.mark(&self.viewer_pong_ms),
        }
    }
    fn since(&self, field: &AtomicU64) -> Duration {
        Duration::from_millis(self.now_ms().saturating_sub(field.load(Ordering::Relaxed)))
    }
}

/// Forward `src` → `dst` until either side closes. Also sends the WebSocket Pings for the
/// `dst` leg (this pump owns its sink) and records Pongs / input arriving on the `src` leg.
async fn pump<S, K>(
    kind: Kind,
    dir: Dir,
    src: &mut S,
    dst: &mut K,
    live: &Liveness,
    traffic: &Traffic,
    t: Timeouts,
) -> EndReason
where
    S: Stream<Item = Result<Message, axum::Error>> + Unpin,
    K: Sink<Message, Error = axum::Error> + Unpin,
{
    let mut ping = interval_at(Instant::now() + t.ping_interval, t.ping_interval);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = src.next() => {
                let msg = match msg {
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => msg,
                    Some(Ok(Message::Pong(_))) => {
                        live.pong(dir);
                        continue;
                    }
                    // tungstenite answers Pings itself.
                    Some(Ok(Message::Ping(_))) => continue,
                    Some(Ok(Message::Close(frame))) => return dir.src_closed(frame.map(|f| f.code)),
                    Some(Err(_)) | None => return dir.src_closed(None),
                };
                if kind.activity(dir, &msg) {
                    live.mark(&live.input_ms);
                }
                traffic.count(dir, &msg);
                if dst.send(msg).await.is_err() {
                    return dir.dst_closed();
                }
            }
            _ = ping.tick() => {
                if dst.send(Message::Ping(Bytes::new())).await.is_err() {
                    return dir.dst_closed();
                }
            }
        }
    }
}

/// Ends the relay when a leg stops answering Pings or there is no activity for the idle
/// timeout.
async fn watchdog(live: &Liveness, t: Timeouts) -> EndReason {
    let period =
        (t.pong_timeout.min(t.idle) / 10).clamp(Duration::from_millis(10), Duration::from_secs(1));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if live.since(&live.viewer_pong_ms) > t.pong_timeout {
            return EndReason::ViewerUnresponsive;
        }
        if live.since(&live.agent_pong_ms) > t.pong_timeout {
            return EndReason::AgentUnresponsive;
        }
        if live.since(&live.input_ms) > t.idle {
            return EndReason::Idle;
        }
    }
}
