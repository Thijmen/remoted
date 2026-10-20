//! File channels (docs/files.md): `/ws/files/{machine}` on the viewer port and the agent's
//! `/ws/agent/files/{channel}` on the agent port, joined by the same relay as screen sessions
//! ([`crate::session::relay`]). Up to [`crate::registry::MAX_FILE_CHANNELS`] per machine, next
//! to (and independent of) its screen session.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rd_proto::files::MAX_MESSAGE;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::auth::ViewerIdentity;
use crate::config::valid_machine_name;
use crate::registry::FilesStartError;
use crate::session::{Kind, agent_auth_failed, relay};
use crate::state::AppState;
use crate::util::{client_ip, close_ws};

/// `GET /ws/files/{machine}` (viewer port; auth already checked by middleware).
pub async fn ws_files(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.write_buffer_size(0)
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run_files(st, viewer, machine, socket))
}

/// `GET /ws/agent/files/{channel}` (agent port).
pub async fn ws_agent_files(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let ident = match st.agent_auth.verify(&headers, ip, "/ws/agent/files").await {
        Ok(ident) => ident,
        Err(rejection) => return agent_auth_failed(&st, rejection),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let (machine, deliver) = match st.registry.claim_files(&channel, ident.machine.as_deref()) {
        Ok(claimed) => claimed,
        Err(err) => {
            warn!(%ip, channel, machine = ident.machine.as_deref(), error = %err, "agent file socket rejected");
            return (StatusCode::NOT_FOUND, "unknown or expired file channel\n").into_response();
        }
    };
    debug!(%ip, machine, channel, "agent file socket accepted");
    ws.write_buffer_size(0)
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            if let Err(socket) = deliver.send(socket) {
                // The viewer gave up between claim and upgrade.
                close_ws(socket, 1000, "file channel ended").await;
            }
        })
}

async fn run_files(st: Arc<AppState>, viewer: ViewerIdentity, machine: String, socket: WebSocket) {
    let _task = st.track_task();
    let ip = viewer.ip.to_string();
    let started = if valid_machine_name(&machine) {
        st.registry
            .start_files(&machine, &viewer.user, &ip, st.cfg.timeouts.session_request)
    } else {
        Err(FilesStartError::Offline)
    };
    let handle = match started {
        Ok(handle) => handle,
        Err(err) => {
            info!(
                user = viewer.user,
                ip,
                machine,
                reason = err.as_str(),
                "file channel refused"
            );
            let (code, reason) = err.viewer_close();
            close_ws(socket, code, reason).await;
            return;
        }
    };
    st.metrics.files_started.inc();
    let channel = handle.id.clone();
    info!(target: "audit", event = "files_open", user = viewer.user, ip, machine, channel, "files_open");
    let begun = Instant::now();
    let (reason, traffic) = relay(&st, Kind::Files, &machine, handle, socket).await;
    let (up, down) = traffic.totals();
    st.metrics.files_bytes_up.add(up);
    st.metrics.files_bytes_down.add(down);
    info!(
        target: "audit",
        event = "files_close",
        user = viewer.user,
        ip,
        machine,
        channel,
        duration_ms = begun.elapsed().as_millis() as u64,
        bytes_up = up,
        bytes_down = down,
        reason = reason.as_str(),
        "files_close"
    );
}
