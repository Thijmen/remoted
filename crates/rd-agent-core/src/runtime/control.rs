//! The control connection (`/ws/agent`): register, report state changes (and with an update
//! hookup `update_status`), receive `session_request` / `session_end` / `update`, reconnect with
//! backoff. Sessions are managed here but run on their own sockets, so they survive control
//! reconnects. While remote access is paused on the machine, it stays disconnected.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use rd_proto::GW_PROTO_VERSION;
use rd_proto::close;
use rd_proto::control::{AgentToGateway, GatewayToAgent, UpdateStatus, agent_caps};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, error, info, warn};

use super::activity::{self, Connection};
use super::backoff::Backoff;
use super::files::Channels as FileChannels;
use super::net::{DialError, WsStream, valid_session_id};
use super::session::{self, Ended, StopReason};
use super::task::TaskGuard;
use super::terminal::Channels as TerminalChannels;
use super::{Ctx, UpdateEvent};

/// Largest control message we accept (the gateway's limit is 64 KiB too).
pub(crate) const MAX_MESSAGE: usize = 64 << 10;
/// A control write that takes longer means the connection is dead.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long shutdown waits for the current session to end cleanly.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// While the proxy answers for a gateway that is not there (restart, redeploy), retry at the
/// shortest backoff for this long before backing off exponentially.
const UNAVAILABLE_FAST_RETRY: Duration = Duration::from_secs(120);
/// Upper bound for a `Retry-After` the gateway asks for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// Runs the control connection until `shutdown` resolves. Shutting down ends the current
/// session first (reported with `session_end` while still connected), then closes the control
/// socket. Pausing (`paused` turning true) does the same, then waits until `paused` is false
/// again.
pub(crate) async fn run(
    ctx: Arc<Ctx>,
    shutdown: impl Future<Output = ()>,
    mut paused: Option<watch::Receiver<bool>>,
) {
    let mut sessions = Sessions::new(ctx.clone());
    let mut files = FileChannels::new(ctx.clone());
    let mut terminals = TerminalChannels::new(ctx.clone());
    let mut backoff = Backoff::new(ctx.timings.backoff_min, ctx.timings.backoff_max);
    let mut unavailable_since: Option<Instant> = None;
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        if paused.as_ref().is_some_and(|p| *p.borrow()) {
            sessions.shutdown().await;
            files.shutdown().await;
            terminals.shutdown().await;
            activity::set_connection(&ctx.activity, Connection::Paused);
            info!("remote access paused on this machine");
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                () = resumed(&mut paused) => {}
            }
            info!("remote access resumed");
            backoff.reset();
            unavailable_since = None;
        }
        // While retrying, the display keeps the reason of the last failure.
        ctx.activity.send_if_modified(|a| {
            let connecting = !matches!(a.connection, Connection::Offline { .. });
            if connecting {
                a.connection = Connection::Connecting;
            }
            connecting
        });
        // `shutdown` is never polled again once it resolved: every path that sees it breaks.
        let mut channels = ChannelSets {
            files: &mut files,
            terminals: &mut terminals,
        };
        let mut stop = std::pin::pin!(stop_signal(shutdown.as_mut(), paused.as_mut()));
        let outcome = connect_and_serve(&ctx, &mut sessions, &mut channels, stop.as_mut()).await;
        if !matches!(outcome, Outcome::Unavailable) {
            unavailable_since = None;
        }
        let delay = match outcome {
            Outcome::Stopped(Stop::Shutdown) => break,
            Outcome::Stopped(Stop::Pause) => continue,
            Outcome::Lost { registered: true } => {
                backoff.reset();
                backoff.next_delay()
            }
            Outcome::Lost { registered: false } | Outcome::Failed => backoff.next_delay(),
            Outcome::Unavailable => {
                let since = *unavailable_since.get_or_insert_with(Instant::now);
                if since.elapsed() < UNAVAILABLE_FAST_RETRY {
                    backoff.min_delay()
                } else {
                    backoff.next_delay()
                }
            }
            Outcome::RateLimited { retry_after } => match retry_after {
                Some(d) => d.clamp(ctx.timings.backoff_min, MAX_RETRY_AFTER),
                None => backoff.max_delay(),
            },
            Outcome::Refused => backoff.max_delay(),
        };
        activity::set_connection(
            &ctx.activity,
            Connection::Offline {
                reason: outcome.reason().into(),
            },
        );
        info!(
            delay_ms = delay.as_millis(),
            "reconnecting to the gateway after a delay"
        );
        tokio::select! {
            stop = &mut stop => match stop {
                Stop::Shutdown => break,
                Stop::Pause => continue,
            },
            () = tokio::time::sleep(delay) => {}
        }
    }
    sessions.shutdown().await;
    files.shutdown().await;
    terminals.shutdown().await;
    // Shells end with the agent (they would be gone with its process anyway).
    ctx.shells.shutdown().await;
}

/// The file and terminal channels (they outlive control connections).
struct ChannelSets<'a> {
    files: &'a mut FileChannels,
    terminals: &'a mut TerminalChannels,
}

/// Why the control connection is being closed on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The runtime is shutting down.
    Shutdown,
    /// Remote access was paused on the machine.
    Pause,
}

/// Resolves when the runtime shuts down or remote access is paused.
async fn stop_signal<F: Future<Output = ()>>(
    shutdown: Pin<&mut F>,
    paused: Option<&mut watch::Receiver<bool>>,
) -> Stop {
    tokio::select! {
        biased;
        () = shutdown => Stop::Shutdown,
        () = pause_requested(paused) => Stop::Pause,
    }
}

async fn pause_requested(paused: Option<&mut watch::Receiver<bool>>) {
    if let Some(paused) = paused
        && paused.wait_for(|p| *p).await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

/// Resolves when remote access is no longer paused. When whoever paused it is gone, it is
/// resumed for good.
async fn resumed(paused: &mut Option<watch::Receiver<bool>>) {
    if let Some(rx) = paused
        && rx.wait_for(|p| !*p).await.is_err()
    {
        *paused = None;
    }
}

/// How a control connection attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Could not connect (network, TLS, timeout).
    Failed,
    /// Connected; the connection was lost later.
    Lost { registered: bool },
    /// An HTTP error that is not about us: the reverse proxy answering 502/503/504/404 while
    /// the gateway restarts, or another 5xx. Retried fast for a while.
    Unavailable,
    /// HTTP 429: too many failed logins from this address; wait as long as asked.
    RateLimited { retry_after: Option<Duration> },
    /// The gateway does not want us (401/403 bad token, wrong machine, replaced by another
    /// agent): retrying fast won't help.
    Refused,
    /// Closed on purpose: the runtime is shutting down, or remote access was paused.
    Stopped(Stop),
}

impl Outcome {
    /// Why the agent is offline, for the status display.
    fn reason(self) -> &'static str {
        match self {
            Outcome::Failed => "cannot reach the gateway",
            Outcome::Lost { .. } => "lost the connection to the gateway",
            Outcome::Unavailable => "the gateway is unavailable",
            Outcome::RateLimited { .. } => "the gateway is rate limiting this network",
            Outcome::Refused => "the gateway refused this agent",
            Outcome::Stopped(_) => "stopped",
        }
    }
}

/// What an HTTP answer to the control upgrade means. Only 401/403 are about the agent itself;
/// everything else (5xx, a 404 from the proxy when the gateway's route is gone, ...) means the
/// gateway is not there right now.
fn classify_http(status: StatusCode, retry_after: Option<Duration>) -> Outcome {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Outcome::Refused,
        StatusCode::TOO_MANY_REQUESTS => Outcome::RateLimited { retry_after },
        _ => Outcome::Unavailable,
    }
}

async fn connect_and_serve<F: Future<Output = Stop>>(
    ctx: &Arc<Ctx>,
    sessions: &mut Sessions,
    channels: &mut ChannelSets<'_>,
    mut stop: Pin<&mut F>,
) -> Outcome {
    let url = ctx.gateway.control_url();
    let dial = timeout(
        ctx.timings.dial_timeout,
        ctx.gateway.dial(&url, MAX_MESSAGE),
    );
    let dialed = tokio::select! {
        stop = &mut stop => return Outcome::Stopped(stop),
        dialed = dial => dialed,
    };
    let ws = match dialed {
        Ok(Ok(ws)) => ws,
        Ok(Err(DialError::Refused {
            status,
            retry_after,
        })) => {
            let outcome = classify_http(status, retry_after);
            match outcome {
                Outcome::Refused => {
                    error!(%status, url, "the gateway refused the agent; check the token");
                }
                Outcome::RateLimited { retry_after } => warn!(
                    %status,
                    url,
                    retry_after_s = retry_after.map(|d| d.as_secs()),
                    "the gateway is rate limiting this address (failed logins)"
                ),
                _ => warn!(%status, url, "the gateway is unavailable; retrying soon"),
            }
            return outcome;
        }
        Ok(Err(DialError::Failed(e))) => {
            warn!("connecting to the gateway failed: {e:#}");
            return Outcome::Failed;
        }
        Err(_) => {
            warn!(url, "connecting to the gateway timed out");
            return Outcome::Failed;
        }
    };
    debug!(url, "control connection open");
    Control {
        ctx,
        sessions,
        files: &mut *channels.files,
        terminals: &mut *channels.terminals,
        ws,
        registered: false,
    }
    .serve(stop)
    .await
}

struct Control<'a> {
    ctx: &'a Arc<Ctx>,
    sessions: &'a mut Sessions,
    files: &'a mut FileChannels,
    terminals: &'a mut TerminalChannels,
    ws: WsStream,
    registered: bool,
}

impl Control<'_> {
    async fn serve<F: Future<Output = Stop>>(mut self, mut stop: Pin<&mut F>) -> Outcome {
        let mut state = self.ctx.state.subscribe();
        // Sent once registered (marked changed then), and after that on every change.
        let mut update_status = self.ctx.updates.as_ref().map(|(status, _)| status.clone());
        let register = AgentToGateway::Register {
            machine: self.ctx.cfg.machine.clone(),
            agent_version: self.ctx.version.clone(),
            os: self.ctx.platform.os_description(),
            gw_proto: GW_PROTO_VERSION,
            state: *state.borrow_and_update(),
            caps: self.caps(),
        };
        if !self.send(&register).await {
            return self.lost();
        }
        // Sessions that ended while we were disconnected: the gateway saw their sockets close.
        self.sessions.discard_ended();
        let mut last_rx = Instant::now();
        loop {
            // Biased: a session's `session_end` goes out before the state change it causes.
            tokio::select! {
                biased;
                stop = &mut stop => return self.shut_down(stop).await,
                msg = self.ws.next() => {
                    last_rx = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if self.on_text(text.as_str())
                                && let Some(status) = update_status.as_mut()
                            {
                                status.mark_changed();
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.as_ref().map(|f| u16::from(f.code));
                            let reason = frame.as_ref().map(|f| f.reason.to_string());
                            return self.on_close(code, reason.as_deref().unwrap_or(""));
                        }
                        // Pings are answered by tungstenite.
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            warn!("control connection failed: {e}");
                            return self.lost();
                        }
                        None => {
                            warn!("control connection closed");
                            return self.lost();
                        }
                    }
                }
                () = sleep_until(last_rx + self.ctx.timings.idle_timeout) => {
                    warn!(
                        idle_s = self.ctx.timings.idle_timeout.as_secs_f32(),
                        "nothing received from the gateway; reconnecting"
                    );
                    return self.lost();
                }
                Some(ended) = self.sessions.next_ended() => {
                    let msg = AgentToGateway::SessionEnd {
                        session: ended.session,
                        reason: ended.reason.into(),
                    };
                    if !self.send(&msg).await {
                        return self.lost();
                    }
                }
                Ok(()) = state.changed() => {
                    let s = *state.borrow_and_update();
                    debug!(?s, "agent state changed");
                    if !self.send(&AgentToGateway::State(s)).await {
                        return self.lost();
                    }
                }
                status = status_changed(&mut update_status), if self.registered => {
                    debug!(phase = ?status.phase, "update status changed");
                    if !self.send(&AgentToGateway::UpdateStatus(status.clone())).await {
                        return self.lost();
                    }
                    self.ctx.gate.reported.send_replace(Some(status));
                }
            }
        }
    }

    /// Ends the session, reports it and hangs up politely.
    async fn shut_down(mut self, stop: Stop) -> Outcome {
        self.sessions.shutdown().await;
        self.files.shutdown().await;
        self.terminals.shutdown().await;
        while let Some(ended) = self.sessions.try_ended() {
            let msg = AgentToGateway::SessionEnd {
                session: ended.session,
                reason: ended.reason.into(),
            };
            if !self.send(&msg).await {
                return Outcome::Stopped(stop);
            }
        }
        let frame = CloseFrame {
            code: CloseCode::Away,
            reason: match stop {
                Stop::Shutdown => "agent shutting down",
                Stop::Pause => "remote access paused on the machine",
            }
            .into(),
        };
        if timeout(SEND_TIMEOUT, self.ws.close(Some(frame)))
            .await
            .is_err()
        {
            debug!("closing the control connection timed out");
        }
        Outcome::Stopped(stop)
    }

    /// Handles a gateway message; true when it was `registered`.
    fn on_text(&mut self, text: &str) -> bool {
        match serde_json::from_str::<GatewayToAgent>(text) {
            Ok(GatewayToAgent::Registered { gw_proto }) => {
                info!(
                    gw_proto,
                    machine = self.ctx.cfg.machine,
                    "registered with the gateway"
                );
                self.registered = true;
                activity::set_connection(&self.ctx.activity, Connection::Online);
                self.update_event(UpdateEvent::Registered {
                    state: *self.ctx.state.borrow(),
                });
                return true;
            }
            Ok(GatewayToAgent::SessionRequest {
                session,
                user,
                client_ip,
            }) => {
                if valid_session_id(&session) {
                    info!(session, user, client_ip, "session requested");
                    self.sessions.start(session, user);
                } else {
                    warn!(session, "ignoring a session request with an invalid id");
                }
            }
            Ok(GatewayToAgent::SessionEnd { session, reason }) => {
                self.sessions.end(&session, reason);
            }
            Ok(GatewayToAgent::Update { version }) => {
                if self.ctx.updates.is_some() {
                    info!(version, "the gateway asks to install an update");
                    self.update_event(UpdateEvent::Install { version });
                } else {
                    warn!(
                        version,
                        "update requested, but this agent cannot update itself"
                    );
                }
            }
            Ok(GatewayToAgent::FilesRequest {
                channel,
                user,
                client_ip,
            }) => {
                if valid_session_id(&channel) {
                    info!(channel, user, client_ip, "file channel requested");
                    self.files.start(channel, user);
                } else {
                    warn!(
                        channel,
                        "ignoring a file channel request with an invalid id"
                    );
                }
            }
            Ok(GatewayToAgent::TerminalRequest {
                channel,
                user,
                client_ip,
            }) => {
                if !self.ctx.terminal {
                    warn!(
                        channel,
                        "ignoring a terminal request: the terminal is off on this machine"
                    );
                } else if valid_session_id(&channel) {
                    info!(channel, user, client_ip, "terminal channel requested");
                    self.terminals.start(channel, user);
                } else {
                    warn!(
                        channel,
                        "ignoring a terminal channel request with an invalid id"
                    );
                }
            }
            Ok(GatewayToAgent::Unknown) => debug!("unknown gateway message ignored"),
            Err(e) => warn!("unparseable gateway message ignored: {e}"),
        }
        false
    }

    /// `register.caps`: files always; the terminal when it is enabled and the platform has
    /// pseudo terminals.
    fn caps(&self) -> Vec<String> {
        let mut caps = vec![agent_caps::FILES.to_owned()];
        if self.ctx.terminal {
            caps.push(agent_caps::TERMINAL.to_owned());
        }
        caps
    }

    fn update_event(&self, event: UpdateEvent) {
        if let Some((_, events)) = &self.ctx.updates
            && events.send(event).is_err()
        {
            debug!("the updater is gone; update event dropped");
        }
    }

    fn on_close(&self, code: Option<u16>, reason: &str) -> Outcome {
        match code {
            Some(close::REPLACED) => {
                warn!(
                    reason,
                    "another agent registered as this machine; backing off"
                );
                Outcome::Refused
            }
            // The machine id does not match the token, or the device was removed or given a new
            // token in the portal (the reason says which): only a new setup helps.
            Some(close::FORBIDDEN) => {
                error!(
                    reason,
                    "the gateway refused this agent; run `rd-agent setup` with a new setup code"
                );
                Outcome::Refused
            }
            _ => {
                info!(code, reason, "the gateway closed the control connection");
                self.lost()
            }
        }
    }

    fn lost(&self) -> Outcome {
        Outcome::Lost {
            registered: self.registered,
        }
    }

    /// False if the connection is unusable.
    async fn send(&mut self, msg: &AgentToGateway) -> bool {
        let text = match serde_json::to_string(msg) {
            Ok(text) => text,
            Err(e) => {
                error!("serializing a control message: {e}");
                return true;
            }
        };
        match timeout(SEND_TIMEOUT, self.ws.send(Message::Text(text.into()))).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                warn!("control connection write failed: {e}");
                false
            }
            Err(_) => {
                warn!("control connection write timed out");
                false
            }
        }
    }
}

/// The next update status, once it changed. Pending forever without an update hookup or once
/// the binary dropped its end.
async fn status_changed(status: &mut Option<watch::Receiver<UpdateStatus>>) -> UpdateStatus {
    if let Some(rx) = status.as_mut()
        && rx.changed().await.is_ok()
    {
        return rx.borrow_and_update().clone();
    }
    std::future::pending().await
}

/// The current session (at most one).
struct Sessions {
    ctx: Arc<Ctx>,
    current: Option<Running>,
    ended_tx: mpsc::UnboundedSender<Ended>,
    ended_rx: mpsc::UnboundedReceiver<Ended>,
}

struct Running {
    id: String,
    stop: Option<oneshot::Sender<StopReason>>,
    task: TaskGuard<()>,
}

impl Running {
    fn stop(&mut self, reason: StopReason) {
        if let Some(stop) = self.stop.take() {
            // Fails if the session already ended by itself.
            let _ = stop.send(reason);
        }
    }
}

impl Sessions {
    fn new(ctx: Arc<Ctx>) -> Self {
        let (ended_tx, ended_rx) = mpsc::unbounded_channel();
        Sessions {
            ctx,
            current: None,
            ended_tx,
            ended_rx,
        }
    }

    /// Starts a session; a running one is stopped and the new one waits for its cleanup.
    fn start(&mut self, id: String, user: String) {
        let predecessor = self.current.take().map(|mut old| {
            if old.stop.is_some() {
                info!(old = old.id, "replacing the current session");
            }
            old.stop(StopReason::Replaced);
            old.task
        });
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = TaskGuard::spawn(session::run(
            self.ctx.clone(),
            id.clone(),
            user,
            stop_rx,
            predecessor,
            self.ended_tx.clone(),
        ));
        self.current = Some(Running {
            id,
            stop: Some(stop_tx),
            task,
        });
    }

    /// The gateway ended session `id`.
    fn end(&mut self, id: &str, reason: String) {
        match self.current.as_mut() {
            Some(current) if current.id == id => current.stop(StopReason::Gateway(reason)),
            _ => debug!(session = id, "session_end for an unknown session ignored"),
        }
    }

    async fn next_ended(&mut self) -> Option<Ended> {
        self.ended_rx.recv().await
    }

    fn try_ended(&mut self) -> Option<Ended> {
        self.ended_rx.try_recv().ok()
    }

    fn discard_ended(&mut self) {
        while self.try_ended().is_some() {}
    }

    /// Stops the current session and waits (bounded) for it to clean up. Idempotent.
    async fn shutdown(&mut self) {
        let Some(mut current) = self.current.take() else {
            return;
        };
        current.stop(StopReason::Shutdown);
        if timeout(SHUTDOWN_GRACE, &mut current.task).await.is_err() {
            warn!("the session did not end in time; aborting it");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_answers_to_the_control_upgrade() {
        // Only a bad token / wrong machine is a refusal.
        assert_eq!(
            classify_http(StatusCode::UNAUTHORIZED, None),
            Outcome::Refused
        );
        assert_eq!(classify_http(StatusCode::FORBIDDEN, None), Outcome::Refused);
        // The proxy while the gateway pod restarts, and other server errors: retry soon.
        for status in [502, 503, 504, 500, 404] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                classify_http(status, None),
                Outcome::Unavailable,
                "{status}"
            );
        }
        let five = Some(Duration::from_secs(5));
        assert_eq!(
            classify_http(StatusCode::TOO_MANY_REQUESTS, five),
            Outcome::RateLimited { retry_after: five }
        );
    }
}
