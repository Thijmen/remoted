//! Side channels next to the screen session, joined by the same relay as sessions
//! ([`crate::session::relay`]):
//!
//! - file channels (docs/files.md): `/ws/files/{machine}` on the viewer port and the agent's
//!   `/ws/agent/files/{channel}` on the agent port;
//! - terminal channels (docs/terminal.md): `/ws/terminal/{machine}` and
//!   `/ws/agent/terminal/{channel}`.
//!
//! Up to [`ChannelKind::max`] of each kind per machine, independent of the session and of each
//! other.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::activity::Event;
use crate::auth::ViewerIdentity;
use crate::config::valid_machine_name;
use crate::registry::{ChannelKind, ChannelStartError};
use crate::session::{Kind, agent_auth_failed, relay};
use crate::state::AppState;
use crate::util::{client_ip, close_ws};

impl ChannelKind {
    /// Largest message either way.
    fn max_message(self) -> usize {
        match self {
            ChannelKind::Files => rd_proto::files::MAX_MESSAGE,
            ChannelKind::Terminal => rd_proto::terminal::MAX_MESSAGE,
        }
    }

    /// The agent-port path prefix, for logs and failed-auth accounting.
    fn agent_path(self) -> &'static str {
        match self {
            ChannelKind::Files => "/ws/agent/files",
            ChannelKind::Terminal => "/ws/agent/terminal",
        }
    }
}

/// `GET /ws/files/{machine}` (viewer port; auth already checked by middleware).
pub async fn ws_files(
    st: State<Arc<AppState>>,
    viewer: Extension<ViewerIdentity>,
    machine: Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws_viewer(ChannelKind::Files, st, viewer, machine, ws)
}

/// `GET /ws/terminal/{machine}` (viewer port; auth already checked by middleware).
pub async fn ws_terminal(
    st: State<Arc<AppState>>,
    viewer: Extension<ViewerIdentity>,
    machine: Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws_viewer(ChannelKind::Terminal, st, viewer, machine, ws)
}

/// `GET /ws/agent/files/{channel}` (agent port).
pub async fn ws_agent_files(
    st: State<Arc<AppState>>,
    peer: ConnectInfo<SocketAddr>,
    channel: Path<String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    ws_agent(ChannelKind::Files, st, peer, channel, headers, ws).await
}

/// `GET /ws/agent/terminal/{channel}` (agent port).
pub async fn ws_agent_terminal(
    st: State<Arc<AppState>>,
    peer: ConnectInfo<SocketAddr>,
    channel: Path<String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    ws_agent(ChannelKind::Terminal, st, peer, channel, headers, ws).await
}

fn ws_viewer(
    kind: ChannelKind,
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let max = kind.max_message();
    ws.write_buffer_size(0)
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| run_channel(kind, st, viewer, machine, socket))
}

async fn ws_agent(
    kind: ChannelKind,
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let ident = match st.agent_auth.verify(&headers, ip, kind.agent_path()).await {
        Ok(ident) => ident,
        Err(rejection) => return agent_auth_failed(&st, rejection),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let (machine, deliver) = match st.registry.claim_channel(
        kind,
        &channel,
        ident.machine.as_deref(),
    ) {
        Ok(claimed) => claimed,
        Err(err) => {
            warn!(%ip, kind = kind.as_str(), channel, machine = ident.machine.as_deref(), error = %err, "agent channel socket rejected");
            return (StatusCode::NOT_FOUND, "unknown or expired channel\n").into_response();
        }
    };
    debug!(%ip, kind = kind.as_str(), machine, channel, "agent channel socket accepted");
    let max = kind.max_message();
    ws.write_buffer_size(0)
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| async move {
            if let Err(socket) = deliver.send(socket) {
                // The viewer gave up between claim and upgrade.
                close_ws(socket, 1000, "channel ended").await;
            }
        })
}

async fn run_channel(
    kind: ChannelKind,
    st: Arc<AppState>,
    viewer: ViewerIdentity,
    machine: String,
    socket: WebSocket,
) {
    let _task = st.track_task();
    let ip = viewer.ip.to_string();
    let started = if valid_machine_name(&machine) {
        st.registry.start_channel(
            kind,
            &machine,
            &viewer.user,
            &ip,
            st.cfg.timeouts.session_request,
        )
    } else {
        Err(ChannelStartError::Offline)
    };
    let handle = match started {
        Ok(handle) => handle,
        Err(err) => {
            info!(
                user = viewer.user,
                ip,
                machine,
                kind = kind.as_str(),
                reason = err.as_str(),
                "channel refused"
            );
            let (code, reason) = err.viewer_close(kind);
            close_ws(socket, code, reason).await;
            return;
        }
    };
    let m = &st.metrics;
    let (started, bytes_up, bytes_down) = match kind {
        ChannelKind::Files => (&m.files_started, &m.files_bytes_up, &m.files_bytes_down),
        ChannelKind::Terminal => (
            &m.terminal_started,
            &m.terminal_bytes_up,
            &m.terminal_bytes_down,
        ),
    };
    started.inc();
    let channel = handle.id.clone();
    let (open, close) = match kind {
        ChannelKind::Files => ("files_open", "files_close"),
        ChannelKind::Terminal => ("terminal_open", "terminal_close"),
    };
    info!(target: "audit", event = open, user = viewer.user, ip, machine, channel, "{open}");
    st.activity
        .record(Event::new(open, &machine).by(&viewer.user));
    let begun = Instant::now();
    let (reason, traffic) = relay(&st, Kind::Channel(kind), &machine, handle, socket).await;
    let (up, down) = traffic.totals();
    bytes_up.add(up);
    bytes_down.add(down);
    info!(
        target: "audit",
        event = close,
        user = viewer.user,
        ip,
        machine,
        channel,
        duration_ms = begun.elapsed().as_millis() as u64,
        bytes_up = up,
        bytes_down = down,
        reason = reason.as_str(),
        "{close}"
    );
}
