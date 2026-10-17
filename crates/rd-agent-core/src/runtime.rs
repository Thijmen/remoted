//! Agent runtime: control connection to the gateway, per-session data connections, the
//! viewer protocol state machine and the flow-control glue (docs/protocol.md).
//!
//! ```text
//! control.rs   /ws/agent: register, state reports, session_request/session_end, reconnect
//!   └─ session.rs   one task per viewer session on its own /ws/agent/session/{id} socket
//!        ├─ reader task   ACK/PING/PONG/input handled inline (never waits on video)
//!        ├─ writer task   sole owner of the socket's sink; control messages jump the video queue
//!        ├─ video.rs      EncodedFrame -> video_config / VIDEO, epochs and flags
//!        ├─ flow.rs       FrameGate on top of rd_flow::FlowGate + BitrateController
//!        └─ input.rs      dedicated thread owning the InputInjector
//! net.rs       TCP (TCP_NODELAY, TCP_NOTSENT_LOWAT) + rustls + WebSocket handshake
//! ```
//!
//! Sessions outlive control reconnects: a joined session keeps streaming on its data socket
//! while the control socket is re-established.

mod backoff;
mod control;
mod flow;
mod input;
mod net;
mod session;
mod task;
mod video;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use rd_proto::control::AgentState;
use tokio::sync::watch;
use tracing::info;

use crate::config::AgentConfig;
use crate::platform::{LockState, Platform};
use net::{DialError, Gateway};
use task::TaskGuard;

/// Version reported in `register` and the viewer `hello`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Timers of the runtime. The defaults are the protocol values from docs/protocol.md; tests
/// shorten them.
#[derive(Debug, Clone, PartialEq)]
pub struct Timings {
    /// How often lock state and permissions are checked (changes are reported at once).
    pub state_poll: Duration,
    /// A socket that received nothing for this long is considered dead (the gateway pings every
    /// 15 s, viewers every 1 s).
    pub idle_timeout: Duration,
    /// Reconnect backoff of the control connection (exponential, ±20 % jitter).
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Connecting a socket: TCP + TLS + WebSocket handshake.
    pub dial_timeout: Duration,
    /// The viewer must send `hello` within this time after the session socket is up.
    pub hello_timeout: Duration,
    /// Oldest in-flight frame older than this: log a stall.
    pub stall_warn: Duration,
    /// Oldest in-flight frame older than this: end the session (close 4006).
    pub stall_end: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            state_poll: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(30),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            dial_timeout: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(10),
            stall_warn: Duration::from_millis(1500),
            stall_end: Duration::from_secs(8),
        }
    }
}

/// The agent runtime. [`run`] is the usual entry point; use this to change [`Timings`] or to
/// shut down gracefully.
pub struct Runtime {
    cfg: AgentConfig,
    platform: Arc<dyn Platform>,
    timings: Timings,
}

impl Runtime {
    pub fn new(cfg: AgentConfig, platform: Arc<dyn Platform>) -> Self {
        Runtime {
            cfg,
            platform,
            timings: Timings::default(),
        }
    }

    pub fn timings(mut self, timings: Timings) -> Self {
        self.timings = timings;
        self
    }

    /// Runs until `shutdown` resolves, then ends the current session cleanly (pipeline stopped,
    /// input released, sockets closed) and returns `Ok(())`. Returns an error only for an
    /// unusable configuration; network failures are retried forever.
    ///
    /// Dropping the future instead aborts everything; resources are still released (the
    /// pipeline is stopped and held input released on drop), but nothing is closed politely.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        let ctx = Arc::new(Ctx::new(self.cfg, self.platform, self.timings)?);
        info!(
            gateway = ctx.cfg.gateway_url,
            machine = ctx.cfg.machine,
            version = AGENT_VERSION,
            "rd agent starting"
        );
        let _monitor = TaskGuard::spawn(monitor_state(ctx.clone()));
        control::run(ctx, shutdown).await;
        info!("rd agent stopped");
        Ok(())
    }
}

/// Run the agent forever (until the future is dropped or a fatal config error occurs).
pub async fn run(cfg: AgentConfig, platform: Arc<dyn Platform>) -> anyhow::Result<()> {
    Runtime::new(cfg, platform)
        .run_until(std::future::pending())
        .await
}

/// Checks that the gateway accepts this agent: connects to `/ws/agent` with the configured
/// token and hangs up again without registering (so a running agent is not replaced).
pub async fn check_gateway(cfg: &AgentConfig) -> anyhow::Result<()> {
    let token = token(cfg)?;
    let gateway = Gateway::new(&cfg.gateway_url, token)?;
    let url = gateway.control_url();
    let dial = gateway.dial(&url, control::MAX_MESSAGE);
    let mut ws = match tokio::time::timeout(Timings::default().dial_timeout, dial).await {
        Err(_) => bail!("timed out connecting to {url}"),
        Ok(Err(DialError::Refused { status, .. })) => {
            bail!("gateway refused the agent: HTTP {status}")
        }
        Ok(Err(DialError::Failed(e))) => return Err(e),
        Ok(Ok(ws)) => ws,
    };
    // Best effort: the check already succeeded once the upgrade was accepted.
    let _ = ws.close(None).await;
    Ok(())
}

fn token(cfg: &AgentConfig) -> anyhow::Result<&str> {
    match cfg.token.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => Ok(t),
        _ => bail!("no agent token configured (config file, RD_AGENT_TOKEN or Keychain)"),
    }
}

/// Machine ids as the gateway accepts them: `[A-Za-z0-9_-]{1,64}`.
fn valid_machine(machine: &str) -> bool {
    (1..=64).contains(&machine.len())
        && machine
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// State shared by the control connection and the session tasks.
struct Ctx {
    cfg: AgentConfig,
    platform: Arc<dyn Platform>,
    timings: Timings,
    gateway: Gateway,
    /// Current agent state; the control connection and the session report changes.
    state: watch::Sender<AgentState>,
    active_sessions: AtomicUsize,
}

impl Ctx {
    fn new(
        cfg: AgentConfig,
        platform: Arc<dyn Platform>,
        timings: Timings,
    ) -> anyhow::Result<Self> {
        ensure!(
            valid_machine(&cfg.machine),
            "invalid machine id {:?} (expected [A-Za-z0-9_-]{{1,64}})",
            cfg.machine
        );
        let gateway = Gateway::new(&cfg.gateway_url, token(&cfg)?)
            .with_context(|| format!("invalid gateway_url {:?}", cfg.gateway_url))?;
        let state = watch::Sender::new(AgentState::default());
        let ctx = Ctx {
            cfg,
            platform,
            timings,
            gateway,
            state,
            active_sessions: AtomicUsize::new(0),
        };
        ctx.refresh_state();
        Ok(ctx)
    }

    /// Re-reads lock state and permissions; subscribers are notified only on change (then
    /// returns true).
    fn refresh_state(&self) -> bool {
        let locked = self.platform.lock_state() == LockState::Locked;
        let perms = self.platform.permissions();
        self.state.send_if_modified(|s| {
            let changed = s.locked != locked || s.perms != perms;
            s.locked = locked;
            s.perms = perms;
            changed
        })
    }

    /// Marks a session as connected until the returned guard is dropped.
    fn session_active(self: &Arc<Self>) -> ActiveSession {
        self.active_sessions.fetch_add(1, Ordering::SeqCst);
        self.publish_in_session();
        ActiveSession(self.clone())
    }

    fn publish_in_session(&self) {
        let in_session = self.active_sessions.load(Ordering::SeqCst) > 0;
        self.state.send_if_modified(|s| {
            let changed = s.in_session != in_session;
            s.in_session = in_session;
            changed
        });
    }
}

/// `AgentState::in_session` is true while at least one of these lives.
struct ActiveSession(Arc<Ctx>);

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.0.active_sessions.fetch_sub(1, Ordering::SeqCst);
        self.0.publish_in_session();
    }
}

async fn monitor_state(ctx: Arc<Ctx>) {
    let mut poll = tokio::time::interval(ctx.timings.state_poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        ctx.refresh_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_ids() {
        assert!(valid_machine("mbp"));
        assert!(valid_machine("desktop-pc_1"));
        assert!(!valid_machine(""));
        assert!(!valid_machine("a/b"));
        assert!(!valid_machine(&"x".repeat(65)));
    }

    #[test]
    fn token_required() {
        let mut cfg = AgentConfig {
            gateway_url: "ws://127.0.0.1:8081".into(),
            machine: "mbp".into(),
            token: None,
            defaults: Default::default(),
        };
        assert!(token(&cfg).is_err());
        cfg.token = Some("  ".into());
        assert!(token(&cfg).is_err());
        cfg.token = Some(" rd1.mbp.x\n".into());
        assert_eq!(token(&cfg).unwrap(), "rd1.mbp.x");
    }
}
