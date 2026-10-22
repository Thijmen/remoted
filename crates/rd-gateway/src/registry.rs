//! Who is online and who is watching: per machine, the agent's control connection, its last
//! known state and update status, at most one viewer session (pending or joined) and its
//! side channels: file channels (docs/files.md) and terminal channels (docs/terminal.md), up to
//! [`ChannelKind::max`] of each, which are independent of the session and of each other.
//!
//! All mutation happens under one short `parking_lot` lock that is never held across an
//! await; messages to agents go through each control socket's bounded queue with `try_send`.

use std::collections::HashMap;

use axum::extract::ws::WebSocket;
use parking_lot::Mutex;
use rd_proto::close;
use rd_proto::control::{
    AgentState, GatewayToAgent, UpdateOutcome, UpdatePhase, UpdateStatus, agent_caps,
};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::warn;

use crate::config::{MachineConfig, RecoveryLink};
use crate::devices::Device;
use crate::util::unix_ms;

/// Capacity of each agent control socket's outgoing queue. Control messages are rare; a full
/// queue means the socket is wedged and the message is reported as undeliverable.
pub const CONTROL_QUEUE: usize = 32;

/// File channels per machine (pending and joined).
pub const MAX_FILE_CHANNELS: usize = 4;

/// Terminal channels per machine (pending and joined).
pub const MAX_TERMINAL_CHANNELS: usize = 8;

/// A side channel next to the screen session: joined like a session, but several per machine
/// and independent of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// Browse and transfer files (docs/files.md).
    Files,
    /// A shell in a terminal (docs/terminal.md).
    Terminal,
}

impl ChannelKind {
    /// Channels of this kind a machine may have at a time.
    pub fn max(self) -> usize {
        match self {
            ChannelKind::Files => MAX_FILE_CHANNELS,
            ChannelKind::Terminal => MAX_TERMINAL_CHANNELS,
        }
    }

    /// The `register.caps` entry that says the agent supports this kind.
    pub fn cap(self) -> &'static str {
        match self {
            ChannelKind::Files => agent_caps::FILES,
            ChannelKind::Terminal => agent_caps::TERMINAL,
        }
    }

    /// For logs and audit lines.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Files => "files",
            ChannelKind::Terminal => "terminal",
        }
    }

    fn request(self, channel: String, user: String, client_ip: String) -> GatewayToAgent {
        match self {
            ChannelKind::Files => GatewayToAgent::FilesRequest {
                channel,
                user,
                client_ip,
            },
            ChannelKind::Terminal => GatewayToAgent::TerminalRequest {
                channel,
                user,
                client_ip,
            },
        }
    }
}

/// `session_end` reason of an agent that ends its session to install an update.
pub const AGENT_UPDATE_REASON: &str = "agent_update";

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
            EndReason::AgentClosed(Some(close::AGENT_UPDATING)) => AGENT_UPDATE_REASON,
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
        match self {
            EndReason::ViewerClosed => (1000, "closed"),
            EndReason::ViewerUnresponsive => (1001, "ping timeout"),
            // The agent ends a session for an update on both sockets: 4010 on the data socket and
            // `session_end` on control. Whichever the gateway sees first, the viewer gets 4010 and
            // knows to wait for the agent to come back.
            EndReason::AgentClosed(Some(close::AGENT_UPDATING)) => {
                (close::AGENT_UPDATING, "agent updating")
            }
            EndReason::AgentEnded(reason) if reason == AGENT_UPDATE_REASON => {
                (close::AGENT_UPDATING, "agent updating")
            }
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
    /// `register.caps`: what the agent supports beyond screen sessions.
    pub caps: Vec<String>,
}

/// Viewer-side handle for a new session or side channel.
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

/// Why a file or terminal channel could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelStartError {
    #[error("agent offline")]
    Offline,
    #[error("the agent does not support this channel")]
    Unsupported,
    #[error("too many channels")]
    Busy,
}

impl ChannelStartError {
    /// Close code + reason sent to the viewer.
    pub fn viewer_close(self, kind: ChannelKind) -> (u16, &'static str) {
        match (self, kind) {
            (ChannelStartError::Offline, _) => (close::AGENT_OFFLINE, "agent offline"),
            (ChannelStartError::Unsupported, ChannelKind::Files) => (
                close::UNSUPPORTED,
                "the agent does not support file transfer",
            ),
            (ChannelStartError::Unsupported, ChannelKind::Terminal) => {
                (close::UNSUPPORTED, "the agent does not offer a terminal")
            }
            (ChannelStartError::Busy, ChannelKind::Files) => {
                (close::TRY_AGAIN_LATER, "too many file channels")
            }
            (ChannelStartError::Busy, ChannelKind::Terminal) => {
                (close::TRY_AGAIN_LATER, "too many terminals")
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChannelStartError::Offline => "agent_offline",
            ChannelStartError::Unsupported => "unsupported",
            ChannelStartError::Busy => "busy",
        }
    }
}

/// Why an update request from the portal was not passed on to the agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateRefusal {
    #[error("no such machine")]
    Unknown,
    #[error("the agent is offline")]
    Offline,
    #[error("this agent does not update itself (rd-agent 1.5.0 and later do)")]
    NotSupported,
    #[error("an update is already in progress")]
    InProgress,
    #[error("{0}")]
    CannotInstall(String),
    #[error("no update is available")]
    NoUpdate,
    #[error("{requested} is not the available update ({available})")]
    OtherVersion {
        requested: String,
        available: String,
    },
}

/// What an `update_status` changed, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChange {
    /// The previous phase, when it changed.
    pub phase_was: Option<UpdatePhase>,
    /// An update outcome the gateway has not logged before.
    pub outcome: Option<UpdateOutcome>,
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

/// Why the gateway closes an agent's control connection from outside its task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kick {
    /// A newer connection registered as the same machine (4009).
    Replaced,
    /// The device was removed or its token replaced (4002, with this reason).
    Revoked(&'static str),
}

struct AgentConn {
    conn_id: u64,
    tx: mpsc::Sender<GatewayToAgent>,
    kick: Option<oneshot::Sender<Kick>>,
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
    /// The agent listed `files` in `register.caps`.
    files: bool,
    /// The agent listed `terminal` in `register.caps`.
    terminal: bool,
}

impl AgentInfo {
    fn supports(&self, kind: ChannelKind) -> bool {
        match kind {
            ChannelKind::Files => self.files,
            ChannelKind::Terminal => self.terminal,
        }
    }
}

/// A side channel and what kind it is.
struct Channel {
    kind: ChannelKind,
    s: Session,
}

#[derive(Default)]
struct Machine {
    agent: Option<AgentConn>,
    /// Last registration (kept while offline).
    info: Option<AgentInfo>,
    /// The agent's last `update_status`. Kept while offline, so the portal still shows an update
    /// that is being installed; cleared by a new registration (see [`Registry::register_agent`]).
    update: Option<UpdateStatus>,
    /// The last update outcome logged. An agent repeats its outcome after every registration;
    /// this keeps it to one log line.
    outcome_logged: Option<UpdateOutcome>,
    /// Online since / offline since, Unix ms.
    since_ms: Option<u64>,
    session: Option<Session>,
    /// Side channels, pending and joined (at most [`ChannelKind::max`] of each kind).
    channels: Vec<Channel>,
}

impl Machine {
    /// End a session and side channels that have not been joined yet: their agent connection
    /// is gone.
    fn fail_pending_session(&mut self) {
        if let Some(s) = self.session.as_mut().filter(|s| !s.joined) {
            s.signal_end(EndReason::AgentOffline);
        }
        for c in self.channels.iter_mut().filter(|c| !c.s.joined) {
            c.s.signal_end(EndReason::AgentOffline);
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
    /// Added in the portal: its token can be replaced and the device removed there.
    pub managed: bool,
    pub online: bool,
    pub since_ms: Option<u64>,
    pub os: Option<String>,
    pub agent_version: Option<String>,
    pub state: Option<AgentState>,
    pub viewer: Option<ViewerView>,
    pub recovery: Vec<RecoveryLink>,
    /// The agent's last `update_status`; `None` for agents that never sent one (before 1.5.0).
    pub update: Option<UpdateStatus>,
    /// The agent supports file channels (`files` in `register.caps`; docs/files.md).
    pub files: bool,
    /// The agent offers a terminal (`terminal` in `register.caps`; docs/terminal.md).
    pub terminal: bool,
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
    /// (4009) through its `kick` channel. Returns the connection id and this connection's own
    /// `kick` receiver.
    ///
    /// The update status is cleared: it describes the agent that sent it, and the new connection
    /// may be another build (the update itself, a rollback, or an older agent installed by hand
    /// that never reports one). Agents that report send `update_status` right after `register`.
    pub fn register_agent(
        &self,
        reg: AgentRegistration,
        tx: mpsc::Sender<GatewayToAgent>,
    ) -> (u64, oneshot::Receiver<Kick>) {
        let mut inner = self.inner.lock();
        inner.next_conn_id += 1;
        let conn_id = inner.next_conn_id;
        let m = inner.machines.entry(reg.machine).or_default();
        if let Some(old) = m.agent.take()
            && let Some(kick) = old.kick
        {
            let _ = kick.send(Kick::Replaced);
        }
        m.fail_pending_session();
        let (kick_tx, kick_rx) = oneshot::channel();
        m.agent = Some(AgentConn {
            conn_id,
            tx,
            kick: Some(kick_tx),
        });
        m.info = Some(AgentInfo {
            os: reg.os,
            agent_version: reg.agent_version,
            state: reg.state,
            files: reg.caps.iter().any(|c| c == ChannelKind::Files.cap()),
            terminal: reg.caps.iter().any(|c| c == ChannelKind::Terminal.cap()),
        });
        m.update = None;
        m.since_ms = Some(unix_ms());
        (conn_id, kick_rx)
    }

    /// A device's token stopped working: close its control connection (4002 with `reason`) and
    /// end its viewer session. With `forget` (the device was removed) the machine also leaves
    /// the portal listing.
    pub fn revoke(&self, machine: &str, reason: &'static str, forget: bool) {
        let mut inner = self.inner.lock();
        let Some(m) = inner.machines.get_mut(machine) else {
            return;
        };
        if let Some(agent) = m.agent.take() {
            if let Some(kick) = agent.kick {
                let _ = kick.send(Kick::Revoked(reason));
            }
            m.since_ms = Some(unix_ms());
        }
        if let Some(s) = m.session.as_mut() {
            s.signal_end(EndReason::AgentOffline);
        }
        for c in &mut m.channels {
            c.s.signal_end(EndReason::AgentOffline);
        }
        if forget {
            inner.machines.remove(machine);
        }
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

    /// Store an agent's `update_status` (only from its current connection). Returns what changed,
    /// or `None` when the connection is not the current one.
    pub fn set_update_status(
        &self,
        machine: &str,
        conn_id: u64,
        status: UpdateStatus,
    ) -> Option<UpdateChange> {
        let mut inner = self.inner.lock();
        let m = inner
            .machines
            .get_mut(machine)
            .filter(|m| m.agent.as_ref().is_some_and(|a| a.conn_id == conn_id))?;
        let was = m.update.as_ref().map_or(UpdatePhase::Idle, |u| u.phase);
        let outcome = status
            .last
            .clone()
            .filter(|last| m.outcome_logged.as_ref() != Some(last));
        if outcome.is_some() {
            m.outcome_logged.clone_from(&status.last);
        }
        let phase_was = (was != status.phase).then_some(was);
        m.update = Some(status);
        Some(UpdateChange { phase_was, outcome })
    }

    /// The portal's Update button: send `update {version}` to the agent if its last status says
    /// it can install exactly that version now. Returns the version the agent runs.
    pub fn request_update(
        &self,
        machine: &str,
        version: &str,
        devices: &[Device],
    ) -> Result<String, UpdateRefusal> {
        let inner = self.inner.lock();
        let Some(m) = inner.machines.get(machine) else {
            let listed = self.configured.iter().any(|c| c.machine == machine)
                || devices.iter().any(|d| d.machine == machine);
            return Err(if listed {
                UpdateRefusal::Offline
            } else {
                UpdateRefusal::Unknown
            });
        };
        let (Some(agent), Some(info)) = (m.agent.as_ref(), m.info.as_ref()) else {
            return Err(UpdateRefusal::Offline);
        };
        let status = m.update.as_ref().ok_or(UpdateRefusal::NotSupported)?;
        if matches!(
            status.phase,
            UpdatePhase::Downloading | UpdatePhase::Installing
        ) {
            return Err(UpdateRefusal::InProgress);
        }
        if !status.can_install {
            return Err(UpdateRefusal::CannotInstall(
                if status.cannot_install.is_empty() {
                    "this agent cannot install updates itself".to_owned()
                } else {
                    status.cannot_install.clone()
                },
            ));
        }
        let available = status.available.as_ref().ok_or(UpdateRefusal::NoUpdate)?;
        if available.version != version {
            return Err(UpdateRefusal::OtherVersion {
                requested: version.to_owned(),
                available: available.version.clone(),
            });
        }
        let update = GatewayToAgent::Update {
            version: version.to_owned(),
        };
        if !send_control(machine, agent, update) {
            return Err(UpdateRefusal::Offline);
        }
        Ok(info.agent_version.clone())
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

    /// Start a side channel for a viewer. Never touches the screen session or other channels.
    pub fn start_channel(
        &self,
        kind: ChannelKind,
        machine: &str,
        user: &str,
        client_ip: &str,
        dial_timeout: std::time::Duration,
    ) -> Result<SessionHandle, ChannelStartError> {
        let mut inner = self.inner.lock();
        let m = inner
            .machines
            .get_mut(machine)
            .ok_or(ChannelStartError::Offline)?;
        let agent = m.agent.as_ref().ok_or(ChannelStartError::Offline)?;
        if !m.info.as_ref().is_some_and(|i| i.supports(kind)) {
            return Err(ChannelStartError::Unsupported);
        }
        if m.channels.iter().filter(|c| c.kind == kind).count() >= kind.max() {
            return Err(ChannelStartError::Busy);
        }
        let id = new_session_id();
        let request = kind.request(id.clone(), user.to_owned(), client_ip.to_owned());
        if !send_control(machine, agent, request) {
            return Err(ChannelStartError::Offline);
        }
        let (deliver_tx, deliver_rx) = oneshot::channel();
        let (end_tx, end_rx) = oneshot::channel();
        let deadline = Instant::now() + dial_timeout;
        m.channels.push(Channel {
            kind,
            s: Session {
                id: id.clone(),
                user: user.to_owned(),
                since_ms: unix_ms(),
                end: Some(end_tx),
                deliver: Some(deliver_tx),
                deadline,
                joined: false,
            },
        });
        Ok(SessionHandle {
            id,
            agent_socket: deliver_rx,
            end: end_rx,
            deadline,
        })
    }

    /// An agent dialed `/ws/agent/files/{channel}` or `/ws/agent/terminal/{channel}`: hand out
    /// the delivery slot if the channel is of that `kind`, pending, not expired and (when
    /// known) belongs to `machine`.
    pub fn claim_channel(
        &self,
        kind: ChannelKind,
        channel: &str,
        machine: Option<&str>,
    ) -> Result<(String, oneshot::Sender<WebSocket>), ClaimError> {
        let mut inner = self.inner.lock();
        let (name, c) = inner
            .machines
            .iter_mut()
            .find_map(|(name, m)| {
                m.channels
                    .iter_mut()
                    .find(|c| c.kind == kind && c.s.id == channel)
                    .map(|c| (name, c))
            })
            .ok_or(ClaimError::Unknown)?;
        if machine.is_some_and(|want| want != name) {
            return Err(ClaimError::WrongMachine);
        }
        if Instant::now() > c.s.deadline {
            return Err(ClaimError::Expired);
        }
        let deliver = c.s.deliver.take().ok_or(ClaimError::AlreadyClaimed)?;
        Ok((name.clone(), deliver))
    }

    /// Mark a side channel as joined; returns the agent's current state.
    pub fn activate_channel(&self, machine: &str, channel: &str) -> AgentState {
        let mut inner = self.inner.lock();
        let Some(m) = inner.machines.get_mut(machine) else {
            return AgentState::default();
        };
        if let Some(c) = m.channels.iter_mut().find(|c| c.s.id == channel) {
            c.s.joined = true;
        }
        m.info.as_ref().map(|i| i.state).unwrap_or_default()
    }

    /// The viewer task is done with a side channel: free its slot. The agent learns from its
    /// socket closing.
    pub fn finish_channel(&self, machine: &str, channel: &str) {
        let mut inner = self.inner.lock();
        if let Some(m) = inner.machines.get_mut(machine) {
            m.channels.retain(|c| c.s.id != channel);
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

    /// Portal listing: configured machines in file order, then devices added in the portal (in
    /// the order they were added), then any other registered agent.
    pub fn machines(&self, devices: &[Device]) -> Vec<MachineView> {
        let inner = self.inner.lock();
        let device = |id: &str| devices.iter().find(|d| d.machine == id);
        let view = |id: &str, cfg: Option<&MachineConfig>| {
            let m = inner.machines.get(id);
            let info = m.and_then(|m| m.info.as_ref());
            MachineView {
                machine: id.to_owned(),
                name: cfg
                    .and_then(|c| c.name.clone())
                    .or_else(|| device(id).and_then(|d| d.name.clone()))
                    .unwrap_or_else(|| id.to_owned()),
                managed: device(id).is_some(),
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
                update: m.and_then(|m| m.update.clone()),
                files: info.is_some_and(|i| i.files),
                terminal: info.is_some_and(|i| i.terminal),
            }
        };
        let configured = |id: &str| self.configured.iter().any(|c| c.machine == id);
        let mut out: Vec<MachineView> = self
            .configured
            .iter()
            .map(|c| view(&c.machine, Some(c)))
            .collect();
        out.extend(
            devices
                .iter()
                .filter(|d| !configured(&d.machine))
                .map(|d| view(&d.machine, None)),
        );
        let mut extra: Vec<&String> = inner
            .machines
            .keys()
            .filter(|id| !configured(id) && device(id).is_none())
            .collect();
        extra.sort();
        out.extend(extra.into_iter().map(|id| view(id, None)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rd_proto::control::{AvailableUpdate, UpdateResult};
    use std::time::Duration;

    fn reg(machine: &str) -> AgentRegistration {
        AgentRegistration {
            machine: machine.into(),
            os: "macOS".into(),
            agent_version: "0.1.0".into(),
            state: AgentState::default(),
            caps: vec![],
        }
    }

    fn reg_files(machine: &str) -> AgentRegistration {
        AgentRegistration {
            caps: vec!["files".into(), "from_the_future".into()],
            ..reg(machine)
        }
    }

    #[tokio::test]
    async fn file_channels_need_the_capability_and_are_limited() {
        let r = Registry::new(vec![]);
        let dial = Duration::from_secs(10);
        assert_eq!(
            r.start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
                .unwrap_err(),
            ChannelStartError::Offline
        );
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg("mbp"), tx);
        assert_eq!(
            r.start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
                .unwrap_err(),
            ChannelStartError::Unsupported
        );
        assert!(!r.machines(&[])[0].files);

        let (tx, mut rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg_files("mbp"), tx);
        assert!(r.machines(&[])[0].files);
        let handles: Vec<_> = (0..MAX_FILE_CHANNELS)
            .map(|_| {
                r.start_channel(ChannelKind::Files, "mbp", "a@x", "1.2.3.4", dial)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            r.start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
                .unwrap_err(),
            ChannelStartError::Busy
        );
        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(msgs.len(), MAX_FILE_CHANNELS);
        assert_eq!(
            msgs[0],
            GatewayToAgent::FilesRequest {
                channel: handles[0].id.clone(),
                user: "a@x".into(),
                client_ip: "1.2.3.4".into()
            }
        );
        // Claiming: the right machine, once; file channel ids are not session ids.
        let id = &handles[1].id;
        assert_eq!(
            r.claim_channel(ChannelKind::Files, id, Some("other"))
                .unwrap_err(),
            ClaimError::WrongMachine
        );
        assert!(r.claim_pending(id, Some("mbp")).is_err());
        assert_eq!(
            r.claim_channel(ChannelKind::Files, id, Some("mbp"))
                .unwrap()
                .0,
            "mbp"
        );
        assert_eq!(
            r.claim_channel(ChannelKind::Files, id, Some("mbp"))
                .unwrap_err(),
            ClaimError::AlreadyClaimed
        );
        assert_eq!(
            r.claim_channel(ChannelKind::Files, "nope", None)
                .unwrap_err(),
            ClaimError::Unknown
        );
        // A finished channel frees its slot.
        r.finish_channel("mbp", &handles[0].id);
        assert!(
            r.start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn file_channels_and_the_session_leave_each_other_alone() {
        let r = Registry::new(vec![]);
        let dial = Duration::from_secs(10);
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg_files("mbp"), tx);
        let session = r.start_session("mbp", "a@x", "ip", dial).unwrap();
        let mut session_end = session.end;
        let files = r
            .start_channel(ChannelKind::Files, "mbp", "b@x", "ip", dial)
            .unwrap();
        let mut files_end = files.end;
        assert!(
            session_end.try_recv().is_err(),
            "a file channel ended the session"
        );
        let _second = r.start_session("mbp", "c@x", "ip", dial).unwrap();
        assert_eq!(session_end.try_recv().unwrap(), EndReason::Kicked);
        assert!(
            files_end.try_recv().is_err(),
            "a session ended a file channel"
        );
        r.finish_channel("mbp", &files.id);
        assert!(r.machines(&[])[0].viewer.is_some());
    }

    #[tokio::test]
    async fn pending_file_channels_fail_with_their_agent_and_revoke_ends_all() {
        let r = Registry::new(vec![]);
        let dial = Duration::from_secs(10);
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        let (id, _kick) = r.register_agent(reg_files("mbp"), tx);
        let joined = r
            .start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
            .unwrap();
        let mut joined_end = joined.end;
        r.claim_channel(ChannelKind::Files, &joined.id, Some("mbp"))
            .unwrap();
        r.activate_channel("mbp", &joined.id);
        let pending = r
            .start_channel(ChannelKind::Files, "mbp", "a@x", "ip", dial)
            .unwrap();
        let mut pending_end = pending.end;
        assert!(r.unregister_agent("mbp", id));
        assert_eq!(pending_end.try_recv().unwrap(), EndReason::AgentOffline);
        assert!(
            joined_end.try_recv().is_err(),
            "a joined channel survives its control socket"
        );
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg_files("mbp"), tx);
        assert!(joined_end.try_recv().is_err());
        r.revoke("mbp", "device removed", false);
        assert_eq!(joined_end.try_recv().unwrap(), EndReason::AgentOffline);
    }

    #[tokio::test]
    async fn expired_file_channels_cannot_be_claimed() {
        let r = Registry::new(vec![]);
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        r.register_agent(reg_files("mbp"), tx);
        let h = r
            .start_channel(
                ChannelKind::Files,
                "mbp",
                "a@x",
                "ip",
                Duration::from_millis(0),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            r.claim_channel(ChannelKind::Files, &h.id, Some("mbp"))
                .unwrap_err(),
            ClaimError::Expired
        );
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
        let (id1, mut kick1) = r.register_agent(reg("mbp"), tx1);
        let pending = r
            .start_session("mbp", "a@x", "ip", Duration::from_secs(10))
            .unwrap();
        let mut pending_end = pending.end;
        let (tx2, _rx2) = mpsc::channel(CONTROL_QUEUE);
        let (id2, _kick2) = r.register_agent(reg("mbp"), tx2);
        assert_eq!(kick1.try_recv(), Ok(Kick::Replaced));
        assert_eq!(pending_end.try_recv().unwrap(), EndReason::AgentOffline);
        assert!(
            !r.unregister_agent("mbp", id1),
            "stale connection must not unregister"
        );
        let list = r.machines(&[]);
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].machine.as_str(), list[0].online), ("pc", false));
        assert_eq!(list[0].name, "PC");
        assert_eq!((list[1].machine.as_str(), list[1].online), ("mbp", true));
        assert!(r.unregister_agent("mbp", id2));
        assert_eq!(r.online_count(), 0);
        assert!(!r.machines(&[])[1].online);
    }

    fn installable(version: &str) -> UpdateStatus {
        UpdateStatus {
            available: Some(AvailableUpdate {
                version: version.into(),
                notes: "- Faster.".into(),
                url: "https://example.test/agent-v1.6.0".into(),
            }),
            can_install: true,
            ..Default::default()
        }
    }

    fn outcome(result: UpdateResult) -> UpdateOutcome {
        UpdateOutcome {
            from: "1.5.0".into(),
            to: "1.6.0".into(),
            result,
            error: String::new(),
            at_ms: 7,
        }
    }

    #[tokio::test]
    async fn update_status_is_stored_kept_offline_and_cleared_by_registration() {
        let r = Registry::new(vec![]);
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        let (id1, _kick) = r.register_agent(reg("mbp"), tx);
        assert_eq!(r.machines(&[])[0].update, None, "nothing reported yet");

        let change = r
            .set_update_status("mbp", id1, installable("1.6.0"))
            .unwrap();
        assert_eq!(
            change,
            UpdateChange {
                phase_was: None,
                outcome: None
            }
        );
        assert_eq!(r.machines(&[])[0].update, Some(installable("1.6.0")));

        let downloading = UpdateStatus {
            phase: UpdatePhase::Downloading,
            ..installable("1.6.0")
        };
        let change = r
            .set_update_status("mbp", id1, downloading.clone())
            .unwrap();
        assert_eq!(change.phase_was, Some(UpdatePhase::Idle));
        assert!(
            r.set_update_status("mbp", id1 + 1, UpdateStatus::default())
                .is_none()
        );

        // Offline: kept, so the portal can show the install in progress.
        assert!(r.unregister_agent("mbp", id1));
        assert_eq!(r.machines(&[])[0].update, Some(downloading));

        // A new registration may be another build: cleared until it reports.
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        let (id2, _kick) = r.register_agent(reg("mbp"), tx);
        assert_eq!(r.machines(&[])[0].update, None);

        // An outcome is reported after every registration but logged once.
        let done = UpdateStatus {
            last: Some(outcome(UpdateResult::Ok)),
            ..Default::default()
        };
        let change = r.set_update_status("mbp", id2, done.clone()).unwrap();
        assert_eq!(change.outcome, Some(outcome(UpdateResult::Ok)));
        assert_eq!(change.phase_was, None, "an empty status counts as idle");
        let (tx, _rx) = mpsc::channel(CONTROL_QUEUE);
        let (id3, _kick) = r.register_agent(reg("mbp"), tx);
        assert_eq!(r.set_update_status("mbp", id3, done).unwrap().outcome, None);
    }

    #[tokio::test]
    async fn update_requests_go_out_only_when_the_agent_can_install_that_version() {
        let r = Registry::new(vec![MachineConfig {
            machine: "pc".into(),
            name: None,
            recovery: vec![],
        }]);
        assert_eq!(
            r.request_update("nope", "1.6.0", &[]),
            Err(UpdateRefusal::Unknown)
        );
        assert_eq!(
            r.request_update("pc", "1.6.0", &[]),
            Err(UpdateRefusal::Offline),
            "configured but never seen"
        );

        let (tx, mut rx) = mpsc::channel(CONTROL_QUEUE);
        let (id, _kick) = r.register_agent(reg("mbp"), tx);
        assert_eq!(
            r.request_update("mbp", "1.6.0", &[]),
            Err(UpdateRefusal::NotSupported)
        );
        let refused = |status: UpdateStatus, version: &str| {
            r.set_update_status("mbp", id, status);
            r.request_update("mbp", version, &[]).unwrap_err()
        };
        assert_eq!(
            refused(UpdateStatus::default(), "1.6.0"),
            UpdateRefusal::CannotInstall("this agent cannot install updates itself".into())
        );
        let by_hand = UpdateStatus {
            can_install: false,
            cannot_install: "not installed by the installer".into(),
            ..installable("1.6.0")
        };
        assert_eq!(
            refused(by_hand, "1.6.0").to_string(),
            "not installed by the installer"
        );
        let up_to_date = UpdateStatus {
            can_install: true,
            ..Default::default()
        };
        assert_eq!(refused(up_to_date, "1.6.0"), UpdateRefusal::NoUpdate);
        assert_eq!(
            refused(installable("1.6.1"), "1.6.0").to_string(),
            "1.6.0 is not the available update (1.6.1)"
        );
        let installing = UpdateStatus {
            phase: UpdatePhase::Installing,
            ..installable("1.6.0")
        };
        assert_eq!(refused(installing, "1.6.0"), UpdateRefusal::InProgress);
        assert!(rx.try_recv().is_err(), "nothing sent so far");

        r.set_update_status("mbp", id, installable("1.6.0"));
        assert_eq!(r.request_update("mbp", "1.6.0", &[]), Ok("0.1.0".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            GatewayToAgent::Update {
                version: "1.6.0".into()
            }
        );

        // A queue that is gone (the agent just disconnected) is reported as offline.
        drop(rx);
        assert_eq!(
            r.request_update("mbp", "1.6.0", &[]),
            Err(UpdateRefusal::Offline)
        );
    }

    #[test]
    fn an_update_ends_the_session_with_4010_whichever_socket_says_so_first() {
        let on_data = EndReason::AgentClosed(Some(close::AGENT_UPDATING));
        let on_control = EndReason::AgentEnded(AGENT_UPDATE_REASON.into());
        for reason in [&on_data, &on_control] {
            assert_eq!(reason.viewer_close().0, close::AGENT_UPDATING);
            assert_eq!(reason.as_str(), AGENT_UPDATE_REASON);
        }
        assert_eq!(
            EndReason::AgentEnded("stall".into()).viewer_close().0,
            close::SESSION_ENDED
        );
    }
}
