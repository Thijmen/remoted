//! Who is online and who is watching: per machine, the agent's control connection, its last
//! known state, and at most one viewer session (pending or joined).
//!
//! All mutation happens under one short `parking_lot` lock that is never held across an
//! await; messages to agents go through each control socket's bounded queue with `try_send`.

use std::collections::HashMap;

use axum::extract::ws::WebSocket;
use parking_lot::Mutex;
use rd_proto::control::{AgentState, GatewayToAgent};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::warn;

use crate::config::{MachineConfig, RecoveryLink};
use crate::util::unix_ms;

/// Capacity of each agent control socket's outgoing queue. Control messages are rare; a full
/// queue means the socket is wedged and the message is reported as undeliverable.
pub const CONTROL_QUEUE: usize = 32;

/// Why a viewer session ended. Determines close codes, `session_end` reasons and audit lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The viewer closed its socket (or it failed).
    ViewerClosed,
    /// No Pong from the viewer within the pong timeout.
    ViewerUnresponsive,
    /// The agent's data socket closed (with the agent's close code, if it sent one) or failed.
    AgentClosed(Option<u16>),
    /// No Pong on the agent's data socket within the pong timeout.
    AgentUnresponsive,
    /// The agent sent `session_end` on its control socket.
    AgentEnded(String),
    /// Another viewer took over the machine.
    Kicked,
    /// No viewer input for the idle timeout.
    Idle,
    /// The agent did not dial its data socket in time.
    AgentDialTimeout,
    /// The agent's control socket went away before the session was joined.
    AgentOffline,
    /// The gateway is shutting down.
    Shutdown,
}

impl EndReason {
    pub fn as_str(&self) -> &str {
        match self {
            EndReason::ViewerClosed => "viewer_closed",
            EndReason::ViewerUnresponsive => "viewer_unresponsive",
            EndReason::AgentClosed(_) => "agent_closed",
            EndReason::AgentUnresponsive => "agent_unresponsive",
            EndReason::AgentEnded(reason) if !reason.is_empty() => reason,
            EndReason::AgentEnded(_) => "agent_ended",
            EndReason::Kicked => "taken_over",
            EndReason::Idle => "idle",
            EndReason::AgentDialTimeout => "agent_timeout",
            EndReason::AgentOffline => "agent_offline",
            EndReason::Shutdown => "gateway_shutdown",
        }
    }

    /// Close code + reason sent to the viewer.
    pub fn viewer_close(&self) -> (u16, &'static str) {
        use rd_proto::close;
        match self {
            EndReason::ViewerClosed => (1000, "closed"),
            EndReason::ViewerUnresponsive => (1001, "ping timeout"),
            // Application codes chosen by the agent (e.g. 4005 protocol) reach the viewer as is.
            EndReason::AgentClosed(Some(code @ 4000..=4999)) => (*code, "closed by agent"),
            EndReason::AgentClosed(_) | EndReason::AgentUnresponsive | EndReason::AgentEnded(_) => {
                (close::SESSION_ENDED, "session ended by agent")
            }
            EndReason::Kicked => (close::KICKED, "taken over by another viewer"),
            EndReason::Idle => (close::IDLE_TIMEOUT, "idle timeout"),
            EndReason::AgentDialTimeout => (close::AGENT_TIMEOUT, "agent did not connect"),
            EndReason::AgentOffline => (close::AGENT_OFFLINE, "agent offline"),
            EndReason::Shutdown => (1012, "gateway restarting"),
        }
    }

    /// Close code + reason sent on the agent's data socket.
    pub fn agent_close(&self) -> (u16, &'static str) {
        match self {
            EndReason::AgentUnresponsive => (1001, "ping timeout"),
            EndReason::Shutdown => (1012, "gateway restarting"),
            _ => (1000, "session ended"),
        }
    }

    /// Whether the gateway tells the agent with `session_end` (not when the agent told us, or
    /// when whoever took over already did).
    fn notify_agent(&self) -> bool {
        !matches!(
            self,
            EndReason::AgentEnded(_) | EndReason::Kicked | EndReason::AgentOffline
        )
    }
}

/// What an agent reported in `register`.
#[derive(Debug, Clone)]
pub struct AgentRegistration {
    pub machine: String,
    pub os: String,
    pub agent_version: String,
    pub state: AgentState,
}

/// Viewer-side handle for a new session.
#[derive(Debug)]
pub struct SessionHandle {
    pub id: String,
    /// Receives the agent's data socket once it dials in.
    pub agent_socket: oneshot::Receiver<WebSocket>,
    /// Receives an end request from elsewhere (take-over, agent `session_end`, agent offline).
    pub end: oneshot::Receiver<EndReason>,
    /// The agent must dial before this.
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("agent offline")]
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    #[error("unknown session")]
    Unknown,
    #[error("session belongs to another machine")]
    WrongMachine,
    #[error("session already joined")]
    AlreadyClaimed,
    #[error("session request expired")]
    Expired,
}

struct AgentConn {
    conn_id: u64,
    tx: mpsc::Sender<GatewayToAgent>,
    replaced: Option<oneshot::Sender<()>>,
}

struct Session {
    id: String,
    user: String,
    since_ms: u64,
    end: Option<oneshot::Sender<EndReason>>,
    /// Hand-off slot for the agent's data socket; `None` once claimed.
    deliver: Option<oneshot::Sender<WebSocket>>,
    deadline: Instant,
    joined: bool,
}

impl Session {
    fn signal_end(&mut self, reason: EndReason) {
        self.deliver = None;
        if let Some(end) = self.end.take() {
            let _ = end.send(reason);
        }
    }
}

#[derive(Clone)]
struct AgentInfo {
    os: String,
    agent_version: String,
    state: AgentState,
}

#[derive(Default)]
struct Machine {
    agent: Option<AgentConn>,
    /// Last registration (kept while offline).
    info: Option<AgentInfo>,
    /// Online since / offline since, Unix ms.
    since_ms: Option<u64>,
    session: Option<Session>,
}

impl Machine {
    /// End a session that has not been joined yet: its agent connection is gone.
    fn fail_pending_session(&mut self) {
        if let Some(s) = self.session.as_mut().filter(|s| !s.joined) {
            s.signal_end(EndReason::AgentOffline);
        }
    }
}

#[derive(Default)]
struct Inner {
    machines: HashMap<String, Machine>,
    next_conn_id: u64,
}

pub struct Registry {
    inner: Mutex<Inner>,
    configured: Vec<MachineConfig>,
}

/// One row of `GET /api/machines`.
#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub machine: String,
    pub name: String,
    pub online: bool,
    pub since_ms: Option<u64>,
    pub os: Option<String>,
    pub agent_version: Option<String>,
    pub state: Option<AgentState>,
    pub viewer: Option<ViewerView>,
    pub recovery: Vec<RecoveryLink>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewerView {
    pub user: String,
    pub since_ms: u64,
}

fn new_session_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Queue a control message for an agent without waiting.
fn send_control(machine: &str, conn: &AgentConn, msg: GatewayToAgent) -> bool {
    match conn.tx.try_send(msg) {
        Ok(()) => true,
        Err(err) => {
            warn!(machine, error = %err, "agent control queue unavailable; message not delivered");
            false
        }
    }
}

impl Registry {
    pub fn new(configured: Vec<MachineConfig>) -> Self {
        Registry {
            inner: Mutex::new(Inner::default()),
            configured,
        }
    }

    /// Register a control connection; a previous one for the same machine is told to close
    /// (4009) through its `replaced` channel. Returns the connection id and this connection's
    /// own `replaced` receiver.
    pub fn register_agent(
        &self,
        reg: AgentRegistration,
        tx: mpsc::Sender<GatewayToAgent>,
    ) -> (u64, oneshot::Receiver<()>) {
        let mut inner = self.inner.lock();
        inner.next_conn_id += 1;
        let conn_id = inner.next_conn_id;
        let m = inner.machines.entry(reg.machine).or_default();
        if let Some(old) = m.agent.take()
            && let Some(replaced) = old.replaced
        {
            let _ = replaced.send(());
        }
        m.fail_pending_session();
        let (replaced_tx, replaced_rx) = oneshot::channel();
        m.agent = Some(AgentConn {
            conn_id,
            tx,
            replaced: Some(replaced_tx),
        });
        m.info = Some(AgentInfo {
            os: reg.os,
            agent_version: reg.agent_version,
            state: reg.state,
        });
        m.since_ms = Some(unix_ms());
        (conn_id, replaced_rx)
    }

    /// Remove a control connection if it is still the current one for `machine`.
    pub fn unregister_agent(&self, machine: &str, conn_id: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(m) = inner.machines.get_mut(machine) else {
            return false;
        };
        if m.agent.as_ref().is_none_or(|a| a.conn_id != conn_id) {
            return false;
        }
        m.agent = None;
        m.since_ms = Some(unix_ms());
        m.fail_pending_session();
        true
    }

    pub fn update_state(&self, machine: &str, conn_id: u64, state: AgentState) {
        let mut inner = self.inner.lock();
        if let Some(m) = inner.machines.get_mut(machine)
            && m.agent.as_ref().is_some_and(|a| a.conn_id == conn_id)
            && let Some(info) = m.info.as_mut()
        {
            info.state = state;
        }
    }

    /// The agent ended `session` on its control socket: tell the viewer task.
    pub fn agent_session_end(&self, machine: &str, session: &str, reason: String) -> bool {
        let mut inner = self.inner.lock();
        match inner
            .machines
            .get_mut(machine)
            .and_then(|m| m.session.as_mut())
            .filter(|s| s.id == session)
        {
            Some(s) => {
                s.signal_end(EndReason::AgentEnded(reason));
                true
            }
            None => false,
        }
    }

    /// Start a session for a viewer: take over any existing one (its viewer gets
    /// [`EndReason::Kicked`] and the agent a `session_end` before the new `session_request`).
    pub fn start_session(
        &self,
        machine: &str,
        user: &str,
        client_ip: &str,
        dial_timeout: std::time::Duration,
    ) -> Result<SessionHandle, StartError> {
        let mut inner = self.inner.lock();
        let m = inner.machines.get_mut(machine).ok_or(StartError::Offline)?;
        let agent = m.agent.as_ref().ok_or(StartError::Offline)?;

        if let Some(mut old) = m.session.take() {
            old.signal_end(EndReason::Kicked);
            send_control(
                machine,
                agent,
                GatewayToAgent::SessionEnd {
                    session: old.id,
                    reason: EndReason::Kicked.as_str().to_owned(),
                },
            );
        }

        let id = new_session_id();
        let request = GatewayToAgent::SessionRequest {
            session: id.clone(),
            user: user.to_owned(),
            client_ip: client_ip.to_owned(),
        };
        if !send_control(machine, agent, request) {
            return Err(StartError::Offline);
        }
        let (deliver_tx, deliver_rx) = oneshot::channel();
        let (end_tx, end_rx) = oneshot::channel();
        let deadline = Instant::now() + dial_timeout;
        m.session = Some(Session {
            id: id.clone(),
            user: user.to_owned(),
            since_ms: unix_ms(),
            end: Some(end_tx),
            deliver: Some(deliver_tx),
            deadline,
            joined: false,
        });
        Ok(SessionHandle {
            id,
            agent_socket: deliver_rx,
            end: end_rx,
            deadline,
        })
    }

    /// An agent dialed `/ws/agent/session/{session}`: hand out the delivery slot if the session
    /// is pending, not expired and (when known) belongs to `machine`.
    pub fn claim_pending(
        &self,
        session: &str,
        machine: Option<&str>,
    ) -> Result<(String, oneshot::Sender<WebSocket>), ClaimError> {
        let mut inner = self.inner.lock();
        let (name, s) = inner
            .machines
            .iter_mut()
            .find_map(|(name, m)| {
                m.session
                    .as_mut()
                    .filter(|s| s.id == session)
                    .map(|s| (name, s))
            })
            .ok_or(ClaimError::Unknown)?;
        if machine.is_some_and(|want| want != name) {
            return Err(ClaimError::WrongMachine);
        }
        if Instant::now() > s.deadline {
            return Err(ClaimError::Expired);
        }
        let deliver = s.deliver.take().ok_or(ClaimError::AlreadyClaimed)?;
        Ok((name.clone(), deliver))
    }

    /// Mark a session as joined; returns the agent's current state for the viewer's `session`
    /// message.
    pub fn activate(&self, machine: &str, session: &str) -> AgentState {
        let mut inner = self.inner.lock();
        let Some(m) = inner.machines.get_mut(machine) else {
            return AgentState::default();
        };
        if let Some(s) = m.session.as_mut().filter(|s| s.id == session) {
            s.joined = true;
        }
        m.info.as_ref().map(|i| i.state).unwrap_or_default()
    }

    /// The viewer task is done with `session`: free the slot (if still ours) and tell the
    /// agent unless it already knows.
    pub fn finish_session(&self, machine: &str, session: &str, reason: &EndReason) {
        let mut inner = self.inner.lock();
        let Some(m) = inner.machines.get_mut(machine) else {
            return;
        };
        if m.session.as_ref().is_none_or(|s| s.id != session) {
            return;
        }
        m.session = None;
        if reason.notify_agent()
            && let Some(agent) = m.agent.as_ref()
        {
            send_control(
                machine,
                agent,
                GatewayToAgent::SessionEnd {
                    session: session.to_owned(),
                    reason: reason.as_str().to_owned(),
                },
            );
        }
    }

    pub fn online_count(&self) -> usize {
        let inner = self.inner.lock();
        inner
            .machines
            .values()
            .filter(|m| m.agent.is_some())
            .count()
    }

    /// Portal listing: configured machines in file order, then any other registered agent.
    pub fn machines(&self) -> Vec<MachineView> {
        let inner = self.inner.lock();
        let view = |id: &str, cfg: Option<&MachineConfig>| {
            let m = inner.machines.get(id);
            let info = m.and_then(|m| m.info.as_ref());
            MachineView {
                machine: id.to_owned(),
                name: cfg
                    .and_then(|c| c.name.clone())
                    .unwrap_or_else(|| id.to_owned()),
                online: m.is_some_and(|m| m.agent.is_some()),
                since_ms: m.and_then(|m| m.since_ms),
                os: info.map(|i| i.os.clone()),
                agent_version: info.map(|i| i.agent_version.clone()),
                state: info.map(|i| i.state),
                viewer: m.and_then(|m| m.session.as_ref()).map(|s| ViewerView {
                    user: s.user.clone(),
                    since_ms: s.since_ms,
                }),
                recovery: cfg.map(|c| c.recovery.clone()).unwrap_or_default(),
            }
        };
        let mut out: Vec<MachineView> = self
            .configured
            .iter()
            .map(|c| view(&c.machine, Some(c)))
            .collect();
        let mut extra: Vec<&String> = inner
            .machines
            .keys()
            .filter(|id| !self.configured.iter().any(|c| &c.machine == *id))
            .collect();
        extra.sort();
        out.extend(extra.into_iter().map(|id| view(id, None)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn reg(machine: &str) -> AgentRegistration {
        AgentRegistration {
            machine: machine.into(),
            os: "macOS".into(),
            agent_version: "0.1.0".into(),
            state: AgentState::default(),
        }
    }

    #[tokio::test]
    async fn take_over_orders_control_messages() {
        let r = Registry::new(vec![]);
        let (tx, mut rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg("mbp"), tx);
        let first = r
            .start_session("mbp", "a@x", "1.1.1.1", Duration::from_secs(10))
            .unwrap();
        let mut first_end = first.end;
        let second = r
            .start_session("mbp", "b@x", "2.2.2.2", Duration::from_secs(10))
            .unwrap();
        assert_eq!(first_end.try_recv().unwrap(), EndReason::Kicked);
        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            msgs,
            vec![
                GatewayToAgent::SessionRequest {
                    session: first.id.clone(),
                    user: "a@x".into(),
                    client_ip: "1.1.1.1".into()
                },
                GatewayToAgent::SessionEnd {
                    session: first.id.clone(),
                    reason: "taken_over".into()
                },
                GatewayToAgent::SessionRequest {
                    session: second.id.clone(),
                    user: "b@x".into(),
                    client_ip: "2.2.2.2".into()
                },
            ]
        );
        // The kicked viewer's cleanup must not touch the new session.
        r.finish_session("mbp", &first.id, &EndReason::Kicked);
        assert!(r.claim_pending(&first.id, Some("mbp")).is_err());
        assert!(r.claim_pending(&second.id, Some("other")).is_err());
        assert!(r.claim_pending(&second.id, Some("mbp")).is_ok());
        assert_eq!(
            r.claim_pending(&second.id, Some("mbp")).unwrap_err(),
            ClaimError::AlreadyClaimed
        );
    }

    #[tokio::test]
    async fn offline_and_replacement() {
        let r = Registry::new(vec![MachineConfig {
            machine: "pc".into(),
            name: Some("PC".into()),
            recovery: vec![],
        }]);
        assert_eq!(
            r.start_session("mbp", "a@x", "ip", Duration::from_secs(1))
                .unwrap_err(),
            StartError::Offline
        );
        let (tx1, _rx1) = mpsc::channel(CONTROL_QUEUE);
        let (id1, mut replaced1) = r.register_agent(reg("mbp"), tx1);
        let pending = r
            .start_session("mbp", "a@x", "ip", Duration::from_secs(10))
            .unwrap();
        let mut pending_end = pending.end;
        let (tx2, _rx2) = mpsc::channel(CONTROL_QUEUE);
        let (id2, _replaced2) = r.register_agent(reg("mbp"), tx2);
        assert!(replaced1.try_recv().is_ok());
        assert_eq!(pending_end.try_recv().unwrap(), EndReason::AgentOffline);
        assert!(
            !r.unregister_agent("mbp", id1),
            "stale connection must not unregister"
        );
        let list = r.machines();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].machine.as_str(), list[0].online), ("pc", false));
        assert_eq!(list[0].name, "PC");
        assert_eq!((list[1].machine.as_str(), list[1].online), ("mbp", true));
        assert!(r.unregister_agent("mbp", id2));
        assert_eq!(r.online_count(), 0);
        assert!(!r.machines()[1].online);
    }
}
