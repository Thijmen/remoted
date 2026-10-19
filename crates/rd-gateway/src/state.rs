//! Shared application state handed to every handler.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::watch;
use tracing::warn;

use crate::assets::Assets;
use crate::auth::AgentAuth;
use crate::config::Config;
use crate::devices::DeviceStore;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::util::{Clock, TaskGuard, TaskTracker};

pub struct AppState {
    pub cfg: Config,
    pub registry: Registry,
    pub devices: Arc<DeviceStore>,
    /// URL agents connect to, for setup codes: `RD_AGENT_PUBLIC_URL`, or in dev mode the agent
    /// listener. `None` turns *Add device* off.
    pub agent_url: Option<String>,
    pub agent_auth: AgentAuth,
    pub assets: Assets,
    pub metrics: Metrics,
    pub clock: Clock,
    tasks: Arc<TaskTracker>,
    shutdown: watch::Receiver<bool>,
}

impl AppState {
    /// `agent_addr`: the bound agent listener (the dev-mode default for setup codes).
    pub fn new(
        cfg: Config,
        shutdown: watch::Receiver<bool>,
        agent_addr: SocketAddr,
    ) -> anyhow::Result<Self> {
        let devices = Arc::new(match &cfg.state_dir {
            Some(dir) => DeviceStore::open(dir)?,
            None if cfg.dev => DeviceStore::in_memory(),
            None => DeviceStore::disabled(),
        });
        let agent_url = cfg
            .agent_public_url
            .clone()
            .or_else(|| cfg.dev.then(|| format!("ws://{agent_addr}")));
        if devices.enabled() && agent_url.is_none() {
            warn!("RD_AGENT_PUBLIC_URL is not set: Add device stays off (setup codes need it)");
        }
        Ok(AppState {
            registry: Registry::new(cfg.machines.clone()),
            agent_auth: AgentAuth::new(cfg.dev, cfg.agent_tokens_dir.clone(), Arc::clone(&devices)),
            devices,
            agent_url,
            assets: Assets::new(cfg.assets_dir.clone()),
            metrics: Metrics::default(),
            clock: Clock::new(),
            tasks: Arc::new(TaskTracker::default()),
            shutdown,
            cfg,
        })
    }

    /// Whether the portal can add devices: a store and a URL to put into setup codes.
    pub fn devices_available(&self) -> bool {
        self.devices.enabled() && self.agent_url.is_some()
    }

    /// Track a long-lived WebSocket task so shutdown waits for it to close its sockets.
    pub fn track_task(&self) -> TaskGuard {
        self.tasks.track()
    }

    pub fn tasks(&self) -> &TaskTracker {
        &self.tasks
    }

    /// Resolves when the gateway starts shutting down (immediately if it already is).
    pub fn shutdown_signal(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.shutdown.clone();
        async move {
            // An error means the sender is gone, which also means shutdown.
            let _ = rx.wait_for(|stopping| *stopping).await;
        }
    }
}
