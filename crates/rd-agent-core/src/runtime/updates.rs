//! The runtime's part in self-updates (docs/updates.md). The binary checks, downloads and
//! installs; the runtime carries messages between it and the gateway, and ends sessions when
//! an update is about to restart the agent.
//!
//! [`link`] makes the two ends: [`UpdateLink`] goes to [`super::Runtime::updates`], the binary
//! keeps [`Updates`].

use std::sync::Arc;
use std::time::Duration;

use rd_proto::control::{AgentState, UpdateStatus};
use tokio::sync::{mpsc, watch};
use tracing::warn;

/// What the runtime tells the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// The gateway accepted this agent (`registered`); `state` is the agent state at that
    /// moment. Comes after every (re)registration.
    Registered { state: AgentState },
    /// The gateway asks to install `version` (the portal's Update button).
    Install { version: String },
}

/// The runtime's end, for [`super::Runtime::updates`].
pub struct UpdateLink {
    pub(crate) status: watch::Receiver<UpdateStatus>,
    pub(crate) events: mpsc::UnboundedSender<UpdateEvent>,
    pub(crate) gate: Arc<SessionGate>,
}

/// The binary's end.
pub struct Updates {
    /// Sent to the gateway as `update_status` after every `registered` and on each change.
    pub status: watch::Sender<UpdateStatus>,
    pub events: mpsc::UnboundedReceiver<UpdateEvent>,
    pub sessions: SessionControl,
}

/// Both ends of the update hookup, starting with `status`.
pub fn link(status: UpdateStatus) -> (UpdateLink, Updates) {
    let (status_tx, status_rx) = watch::channel(status);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(SessionGate::default());
    (
        UpdateLink {
            status: status_rx,
            events: events_tx,
            gate: gate.clone(),
        },
        Updates {
            status: status_tx.clone(),
            events: events_rx,
            sessions: SessionControl {
                gate,
                status: status_tx,
            },
        },
    )
}

/// Ends sessions for an update.
#[derive(Clone)]
pub struct SessionControl {
    gate: Arc<SessionGate>,
    status: watch::Sender<UpdateStatus>,
}

impl SessionControl {
    /// Ends the current session and turns new ones away until the returned hold is dropped:
    /// their data sockets are closed with 4010 (`AGENT_UPDATING`, so viewers keep reconnecting)
    /// and reported with `session_end` reason `agent_update`.
    ///
    /// First waits (at most `wait`) until the current update status has been sent to the
    /// gateway: a viewer that sees 4010 asks the gateway whether the agent is updating, and
    /// must not find it idle and reconnect to the old agent. Then waits at most `wait` for the
    /// current session to finish.
    pub async fn hold(&self, wait: Duration) -> SessionHold {
        let phase = self.status.borrow().phase;
        let mut reported = self.gate.reported.subscribe();
        if tokio::time::timeout(
            wait,
            reported.wait_for(|r| r.as_ref().is_some_and(|r| r.phase == phase)),
        )
        .await
        .is_err()
        {
            warn!("the update status was not reported in time; ending the session anyway");
        }
        self.gate.holds.send_modify(|n| *n += 1);
        let hold = SessionHold(self.gate.clone());
        let mut sessions = self.gate.sessions.subscribe();
        if tokio::time::timeout(wait, sessions.wait_for(|n| *n == 0))
            .await
            .is_err()
        {
            warn!("the session did not end in time for the update");
        }
        hold
    }
}

/// Sessions end and new ones are turned away while one of these lives.
pub struct SessionHold(Arc<SessionGate>);

impl Drop for SessionHold {
    fn drop(&mut self) {
        self.0.holds.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Shared by the runtime's sessions and [`SessionControl::hold`].
pub(crate) struct SessionGate {
    /// Live [`SessionHold`]s.
    pub(crate) holds: watch::Sender<usize>,
    /// Sessions with a connected data socket.
    pub(crate) sessions: watch::Sender<usize>,
    /// The update status last sent to the gateway.
    pub(crate) reported: watch::Sender<Option<UpdateStatus>>,
}

impl Default for SessionGate {
    fn default() -> Self {
        SessionGate {
            holds: watch::Sender::new(0),
            sessions: watch::Sender::new(0),
            reported: watch::Sender::new(None),
        }
    }
}

impl SessionGate {
    pub(crate) fn held(&self) -> watch::Receiver<usize> {
        self.holds.subscribe()
    }
}
