//! Agent control socket `/ws/agent`: registration, state and update status reports, session
//! requests/ends, update requests.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use rd_proto::control::{AgentToGateway, GatewayToAgent, UpdatePhase, UpdateResult, UpdateStatus};
use rd_proto::{GW_PROTO_VERSION, close};
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::activity::Event;
use crate::auth::AgentIdentity;
use crate::config::valid_machine_name;
use crate::registry::{AgentRegistration, CONTROL_QUEUE, Kick};
use crate::session::agent_auth_failed;
use crate::state::AppState;
use crate::util::{client_ip, close_ws};

/// Control messages are small JSON documents.
const CONTROL_MAX_MESSAGE: usize = 64 << 10;

/// `GET /ws/agent` (agent port).
pub async fn ws_agent(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let ident = match st.agent_auth.verify(&headers, ip, "/ws/agent").await {
        Ok(ident) => ident,
        Err(rejection) => return agent_auth_failed(&st, rejection),
    };
    match ws {
        Ok(ws) => ws
            .write_buffer_size(0)
            .max_message_size(CONTROL_MAX_MESSAGE)
            .max_frame_size(CONTROL_MAX_MESSAGE)
            .on_upgrade(move |socket| run_control(st, ident, ip, socket)),
        Err(rejection) => rejection.into_response(),
    }
}

/// Why a control connection ended.
enum Exit {
    /// The agent closed it (or the socket failed).
    Peer,
    /// We close it with this code and reason.
    Close(u16, &'static str),
}

async fn run_control(st: Arc<AppState>, ident: AgentIdentity, ip: IpAddr, mut socket: WebSocket) {
    let _task = st.track_task();
    let t = st.cfg.timeouts;
    let mut shutdown = std::pin::pin!(st.shutdown_signal());

    let first = tokio::select! {
        () = &mut shutdown => Err((1012, "gateway restarting")),
        r = tokio::time::timeout(t.register, recv_register(&mut socket)) => match r {
            Ok(r) => r.map_err(|reason| (close::PROTOCOL, reason)),
            Err(_) => Err((close::PROTOCOL, "register timeout")),
        },
    };
    let reg = match first {
        Ok(reg) => reg,
        Err((code, reason)) => {
            warn!(%ip, code, reason, "agent control: not registered");
            close_ws(socket, code, reason).await;
            return;
        }
    };
    if let Some(token_machine) = &ident.machine
        && *token_machine != reg.machine
    {
        warn!(%ip, token_machine, machine = reg.machine, "agent control: machine does not match token");
        close_ws(socket, close::FORBIDDEN, "machine does not match token").await;
        return;
    }

    let machine = reg.machine.clone();
    info!(%ip, machine, os = reg.os, agent_version = reg.agent_version, caps = ?reg.caps, "agent registered");
    let previous = st.registry.agent_version(&machine);
    let version = reg.agent_version.clone();
    // Register before acknowledging, so the machine is online once the agent hears back.
    let (tx, mut outbox) = mpsc::channel(CONTROL_QUEUE);
    let (conn_id, mut kick) = st.registry.register_agent(reg, tx);
    st.metrics.agent_registrations.inc();
    st.activity.record(Event::new("agent_online", &machine));
    // Back with another version: an update (or a manual install) went through.
    if let Some(previous) = previous
        && !previous.is_empty()
        && !version.is_empty()
        && previous != version
    {
        st.activity
            .record(Event::new("agent_updated", &machine).versions(&previous, &version));
    }
    let registered = serde_json::to_string(&GatewayToAgent::Registered {
        gw_proto: GW_PROTO_VERSION,
    })
    .expect("registered serializes");
    let exit = if socket.send(Message::Text(registered.into())).await.is_ok() {
        serve(
            &st,
            &machine,
            conn_id,
            &mut socket,
            &mut outbox,
            &mut kick,
            shutdown,
        )
        .await
    } else {
        Exit::Peer
    };

    let current = st.registry.unregister_agent(&machine, conn_id);
    if current {
        st.activity.record(Event::new("agent_offline", &machine));
    }
    match exit {
        Exit::Close(code, reason) => {
            info!(%ip, machine, code, reason, "agent control closed by gateway");
            close_ws(socket, code, reason).await;
        }
        Exit::Peer => info!(%ip, machine, offline = current, "agent control disconnected"),
    }
}

/// The registered control loop: forward queued control messages, keep the socket alive and
/// apply what the agent reports.
async fn serve(
    st: &AppState,
    machine: &str,
    conn_id: u64,
    socket: &mut WebSocket,
    outbox: &mut mpsc::Receiver<GatewayToAgent>,
    kick: &mut oneshot::Receiver<Kick>,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> Exit {
    let t = st.cfg.timeouts;
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + t.ping_interval,
        t.ping_interval,
    );
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_pong = Instant::now();
    loop {
        tokio::select! {
            biased;
            kicked = &mut *kick => return match kicked {
                Ok(Kick::Revoked(reason)) => Exit::Close(close::FORBIDDEN, reason),
                Ok(Kick::Replaced) | Err(_) => Exit::Close(close::REPLACED, "replaced by a newer connection"),
            },
            () = &mut shutdown => return Exit::Close(1012, "gateway restarting"),
            cmd = outbox.recv() => {
                // The registry only drops our sender when replacing us (handled above).
                let Some(cmd) = cmd else { return Exit::Close(close::REPLACED, "replaced") };
                let text = serde_json::to_string(&cmd).expect("control message serializes");
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return Exit::Peer;
                }
            }
            _ = ping.tick() => {
                if last_pong.elapsed() > t.pong_timeout {
                    warn!(machine, "agent control: ping timeout");
                    return Exit::Close(1001, "ping timeout");
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return Exit::Peer;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => handle_message(st, machine, conn_id, &text),
                Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                Some(Ok(Message::Binary(_) | Message::Ping(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Exit::Peer,
            },
        }
    }
}

fn handle_message(st: &AppState, machine: &str, conn_id: u64, text: &str) {
    match serde_json::from_str::<AgentToGateway>(text) {
        Ok(AgentToGateway::State(state)) => st.registry.update_state(machine, conn_id, state),
        Ok(AgentToGateway::SessionEnd { session, reason }) => {
            info!(machine, session, reason, "agent ended session");
            st.registry.agent_session_end(machine, &session, reason);
        }
        Ok(AgentToGateway::UpdateStatus(status)) => update_status(st, machine, conn_id, status),
        Ok(AgentToGateway::Register { .. }) => {
            warn!(machine, "agent control: duplicate register ignored");
        }
        Ok(AgentToGateway::Unknown) => debug!(machine, "agent control: unknown message ignored"),
        Err(err) => warn!(machine, error = %err, "agent control: unparseable message ignored"),
    }
}

/// Store an agent's update status. Every report is logged at debug; a new phase and a new outcome
/// at info, so the log tells the story of an update without the 6-hourly repeats.
fn update_status(st: &AppState, machine: &str, conn_id: u64, status: UpdateStatus) {
    let available = status.available.as_ref().map(|a| a.version.clone());
    let phase = status.phase;
    debug!(
        machine,
        phase = phase_name(phase),
        available,
        can_install = status.can_install,
        cannot_install = status.cannot_install,
        error = status.error,
        checked_ms = status.checked_ms,
        "agent update status"
    );
    let Some(change) = st.registry.set_update_status(machine, conn_id, status) else {
        return;
    };
    if let Some(was) = change.phase_was {
        info!(
            machine,
            phase = phase_name(phase),
            was = phase_name(was),
            available,
            "agent update phase"
        );
    }
    if let Some(o) = change.outcome {
        info!(
            machine,
            from = o.from,
            to = o.to,
            result = result_name(o.result),
            error = o.error,
            at_ms = o.at_ms,
            "agent update outcome"
        );
    }
}

/// The wire name (`update_status.phase`), for logs.
fn phase_name(phase: UpdatePhase) -> &'static str {
    match phase {
        UpdatePhase::Idle => "idle",
        UpdatePhase::Checking => "checking",
        UpdatePhase::Downloading => "downloading",
        UpdatePhase::Installing => "installing",
        UpdatePhase::Unknown => "unknown",
    }
}

/// The wire name (`update_status.last.result`), for logs.
fn result_name(result: UpdateResult) -> &'static str {
    match result {
        UpdateResult::Ok => "ok",
        UpdateResult::RolledBack => "rolled_back",
        UpdateResult::Failed => "failed",
        UpdateResult::Unknown => "unknown",
    }
}

/// Wait for the first message, which must be a valid `register`.
async fn recv_register(socket: &mut WebSocket) -> Result<AgentRegistration, &'static str> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                return match serde_json::from_str::<AgentToGateway>(&text) {
                    Ok(AgentToGateway::Register {
                        machine,
                        agent_version,
                        os,
                        gw_proto,
                        state,
                        caps,
                    }) => {
                        if gw_proto == 0 {
                            Err("unsupported gw_proto")
                        } else if !valid_machine_name(&machine) {
                            Err("invalid machine id")
                        } else {
                            Ok(AgentRegistration {
                                machine,
                                os,
                                agent_version,
                                state,
                                caps,
                            })
                        }
                    }
                    _ => Err("first message must be register"),
                };
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return Err("expected register"),
        }
    }
}
