//! Shared application state handed to every handler.

use std::sync::Arc;

use tokio::sync::watch;

use crate::assets::Assets;
use crate::auth::AgentAuth;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::util::{Clock, TaskGuard, TaskTracker};

pub struct AppState {
    pub cfg: Config,
    pub registry: Registry,
    pub agent_auth: AgentAuth,
    pub assets: Assets,
    pub metrics: Metrics,
    pub clock: Clock,
    tasks: Arc<TaskTracker>,
    shutdown: watch::Receiver<bool>,
}

impl AppState {
    pub fn new(cfg: Config, shutdown: watch::Receiver<bool>) -> Self {
        AppState {
            registry: Registry::new(cfg.machines.clone()),
            agent_auth: AgentAuth::new(cfg.dev, cfg.agent_tokens_dir.clone()),
            assets: Assets::new(cfg.assets_dir.clone()),
            metrics: Metrics::default(),
            clock: Clock::new(),
            tasks: Arc::new(TaskTracker::default()),
            shutdown,
            cfg,
        }
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
