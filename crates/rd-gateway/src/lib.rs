//! rd-gateway: joins viewer and agent WebSockets, serves the portal/viewer build and the
//! network probe. Operator notes: `docs/gateway.md`; contracts: `docs/protocol.md`.
//!
//! Listeners:
//! - viewer (8080): pages, `/api/*`, `/ws/view/{machine}`, `/ws/probe` — behind Authentik,
//!   checked against the email allowlist and Origin.
//! - agent (8081): `/ws/agent` (control) and `/ws/agent/session/{session}` (data) — bearer
//!   tokens.
//! - metrics (9090): `/healthz`, `/metrics`.

mod agent;
mod assets;
mod auth;
mod chaos;
mod config;
mod devices;
mod http;
mod metrics;
mod probe;
mod registry;
mod server;
mod session;
mod state;
mod util;

pub use chaos::Chaos;
pub use config::{Cli, Config, MachineConfig, RecoveryLink, Timeouts};
pub use server::Gateway;
