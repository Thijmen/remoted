//! Binding the listeners, serving, and graceful shutdown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::http::{agent_router, metrics_router, viewer_router};
use crate::state::AppState;

/// How long shutdown waits for sockets to close and HTTP requests to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// A gateway with its listeners bound (ports are known before serving, which tests use).
pub struct Gateway {
    state: Arc<AppState>,
    shutdown: watch::Sender<bool>,
    viewer: TcpListener,
    agent: TcpListener,
    metrics: TcpListener,
}

async fn bind(addr: SocketAddr, what: &str) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {what} listener on {addr}"))
}

impl Gateway {
    pub async fn bind(cfg: Config) -> anyhow::Result<Self> {
        let viewer = bind(cfg.viewer_addr, "viewer").await?;
        let agent = bind(cfg.agent_addr, "agent").await?;
        let metrics = bind(cfg.metrics_addr, "metrics").await?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let agent_addr = agent.local_addr().context("agent listener address")?;
        Ok(Gateway {
            state: Arc::new(AppState::new(cfg, shutdown_rx, agent_addr)?),
            shutdown,
            viewer,
            agent,
            metrics,
        })
    }

    pub fn viewer_addr(&self) -> SocketAddr {
        self.viewer
            .local_addr()
            .expect("bound listener has an address")
    }

    pub fn agent_addr(&self) -> SocketAddr {
        self.agent
            .local_addr()
            .expect("bound listener has an address")
    }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.metrics
            .local_addr()
            .expect("bound listener has an address")
    }

    /// Serve until `shutdown` resolves, then close every WebSocket (viewers and agents get
    /// 1012 so they reconnect) and drain HTTP requests for up to [`SHUTDOWN_GRACE`].
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> anyhow::Result<()> {
        let st = self.state;
        let cfg = &st.cfg;
        info!(
            dev = cfg.dev,
            viewer = %self.viewer.local_addr()?,
            agent = %self.agent.local_addr()?,
            metrics = %self.metrics.local_addr()?,
            assets = st.assets.describe(),
            machines = cfg.machines.len(),
            devices = st.devices.describe(),
            agent_url = st.agent_url.as_deref().unwrap_or("-"),
            chaos = ?cfg.chaos,
            "rd-gateway listening"
        );
        let mut servers = vec![
            spawn_server(&st, self.viewer, viewer_router(Arc::clone(&st))),
            spawn_server(&st, self.agent, agent_router(Arc::clone(&st))),
            spawn_server(&st, self.metrics, metrics_router(Arc::clone(&st))),
        ];

        let failed = tokio::select! {
            () = shutdown => None,
            (res, idx, _) = futures_util::future::select_all(servers.iter_mut()) => Some((idx, res)),
        };
        let _ = self.shutdown.send(true);
        if let Some((idx, res)) = failed {
            servers.remove(idx);
            let err = match res {
                Ok(Ok(())) => anyhow::anyhow!("listener stopped unexpectedly"),
                Ok(Err(err)) => anyhow::Error::new(err).context("listener failed"),
                Err(err) => anyhow::Error::new(err).context("listener task panicked"),
            };
            warn!(error = %err, "shutting down after listener failure");
            drain(&st, servers).await;
            return Err(err);
        }
        info!(live_sockets = st.tasks().live(), "shutting down");
        drain(&st, servers).await;
        info!("shutdown complete");
        Ok(())
    }
}

type ServerTask = JoinHandle<std::io::Result<()>>;

fn spawn_server(st: &Arc<AppState>, listener: TcpListener, router: Router) -> ServerTask {
    let listener = listener.tap_io(|tcp| {
        if let Err(err) = tcp.set_nodelay(true) {
            debug!(error = %err, "failed to set TCP_NODELAY");
        }
    });
    let stop = st.shutdown_signal();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop)
        .await
    })
}

async fn drain(st: &AppState, servers: Vec<ServerTask>) {
    let all = async {
        st.tasks().wait_idle().await;
        for server in servers {
            let _ = server.await;
        }
    };
    if tokio::time::timeout(SHUTDOWN_GRACE, all).await.is_err() {
        warn!(
            live_sockets = st.tasks().live(),
            "shutdown grace period expired"
        );
    }
}
