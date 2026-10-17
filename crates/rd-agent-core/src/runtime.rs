//! Agent runtime: control connection to the gateway, per-session data connections, the
//! viewer protocol state machine and the flow-control glue.
//!
//! TO BE IMPLEMENTED (see docs/protocol.md and the module docs in rd-flow):
//! - `run(cfg, platform)`: connect `<gateway_url>/ws/agent` with `Authorization: Bearer <token>`,
//!   send `register`, handle `session_request` by dialing `<gateway_url>/ws/agent/session/<id>`
//!   (fresh TCP+TLS connection per session), report `state` changes (lock/perms) every 2 s when
//!   changed, reconnect with jittered exponential backoff 1-30 s, reconnect if no pong in 30 s.
//! - Session: viewer `hello` -> agent `hello`; viewer `start` -> create VideoPipeline + FlowGate +
//!   BitrateController, keep-awake guard, send `video_config` before each new epoch's first frame,
//!   VIDEO frames, `stats` every 1 s, PING every 500 ms (RTT -> FlowGate), answer viewer PINGs
//!   with PONG, ACK handling, `keyframe_req` (rate-limited 1 / 500 ms), `pause`/`resume`, `set`,
//!   input events -> dedicated input thread -> InputInjector, RELEASE_ALL on session end.
//! - Socket options: TCP_NODELAY, TCP_NOTSENT_LOWAT = 32 KiB (macOS 0x201 / Linux 25).

use std::sync::Arc;

use crate::config::AgentConfig;
use crate::platform::Platform;

/// Run the agent forever (until the future is dropped or a fatal config error occurs).
pub async fn run(cfg: AgentConfig, platform: Arc<dyn Platform>) -> anyhow::Result<()> {
    let _ = (cfg, platform);
    todo!("rd-agent-core: runtime::run")
}
