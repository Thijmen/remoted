//! Agent updates: `update_status` in the portal listing, the portal's update request reaching the
//! agent, its refusals and its audit line (production mode, so the viewer checks apply).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rd_gateway::{Config, MachineConfig};
use rd_proto::control::{AgentToGateway, AvailableUpdate, UpdatePhase, UpdateStatus};
use tokio_tungstenite::tungstenite::Message;
use tracing_subscriber::fmt::MakeWriter;

const ORIGIN: &str = "https://rd.example.test";
const EMAIL: &str = "me@example.test";
const TOKEN: &str = "rd1.mbp.secret";
const VIEWER: (&str, &str) = ("x-authentik-email", EMAIL);
const JSON: (&str, &str) = ("content-type", "application/json");

fn config(tokens: &TokenDir) -> Config {
    Config {
        dev: false,
        public_origin: Some(ORIGIN.into()),
        allowed_emails: [EMAIL.to_owned()].into_iter().collect(),
        agent_tokens_dir: tokens.path.clone(),
        machines: vec![MachineConfig {
            machine: "pc".into(),
            name: None,
            recovery: vec![],
        }],
        ..dev_config()
    }
}

fn installable(version: &str) -> UpdateStatus {
    UpdateStatus {
        available: Some(AvailableUpdate {
            version: version.into(),
            notes: "## 1.6.0\n\n- Faster.".into(),
            url: "https://github.com/Thijmen/remoted/releases/tag/agent-v1.6.0".into(),
        }),
        can_install: true,
        ..Default::default()
    }
}

async fn request(gw: &TestGateway, machine: &str, body: &str) -> HttpResponse {
    http(
        gw.viewer,
        "POST",
        &format!("/api/machines/{machine}/update"),
        &[VIEWER, JSON, ("origin", ORIGIN)],
        body.into(),
    )
    .await
}

fn error(r: &HttpResponse) -> String {
    r.json()["error"].as_str().unwrap_or_default().to_owned()
}

/// `machine`'s row in the portal listing.
async fn listed(gw: &TestGateway, machine: &str) -> serde_json::Value {
    let list = get(gw.viewer, "/api/machines", &[VIEWER]).await.json();
    list.as_array()
        .unwrap()
        .iter()
        .find(|m| m["machine"] == machine)
        .unwrap_or_else(|| panic!("{machine} not listed"))
        .clone()
}

async fn listed_update(gw: &TestGateway, machine: &str) -> serde_json::Value {
    listed(gw, machine).await["update"].clone()
}

/// Report `status` and wait until the portal shows it (the control socket is processed
/// asynchronously).
async fn report(gw: &TestGateway, agent: &mut FakeAgent, status: UpdateStatus) {
    let want = serde_json::to_value(&status).unwrap();
    send_json(&mut agent.ctl, &AgentToGateway::UpdateStatus(status)).await;
    for _ in 0..100 {
        if listed_update(gw, "mbp").await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("update status never showed up: {want}");
}

/// Log output of the in-process gateway (JSON lines, like production).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn lines(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

// A current-thread runtime: the gateway's tasks run on this thread, so the thread's log
// subscriber sees them.
#[tokio::test]
async fn update_request_reaches_the_agent_and_is_audited() {
    let logs = Logs::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .finish(),
    );
    let tokens = TokenDir::new("updates");
    tokens.add("mbp", TOKEN);
    let gw = TestGateway::start(config(&tokens)).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();

    // An agent that never reports (before 1.5.0) has no update status and cannot be updated.
    assert!(listed_update(&gw, "mbp").await.is_null());
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(r.status, 409);
    assert!(
        error(&r).contains("does not update itself"),
        "{}",
        error(&r)
    );

    report(&gw, &mut agent, installable("1.6.0")).await;
    assert_eq!(
        listed_update(&gw, "mbp").await,
        serde_json::json!({
            "available": {
                "version": "1.6.0",
                "notes": "## 1.6.0\n\n- Faster.",
                "url": "https://github.com/Thijmen/remoted/releases/tag/agent-v1.6.0"
            },
            "can_install": true,
            "phase": "idle"
        })
    );

    // Refusals: another version, unknown and offline machines, non-JSON and cross-site bodies.
    let r = request(&gw, "mbp", r#"{"version":"1.5.9"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "1.5.9 is not the available update (1.6.0)")
    );
    assert_eq!(
        request(&gw, "nope", r#"{"version":"1.6.0"}"#).await.status,
        404
    );
    let r = request(&gw, "pc", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "the agent is offline")
    );
    let form = http(
        gw.viewer,
        "POST",
        "/api/machines/mbp/update",
        &[
            VIEWER,
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        b"version=1.6.0".to_vec(),
    )
    .await;
    assert_eq!(form.status, 415);
    let foreign = http(
        gw.viewer,
        "POST",
        "/api/machines/mbp/update",
        &[VIEWER, JSON, ("origin", "https://evil.example.test")],
        br#"{"version":"1.6.0"}"#.to_vec(),
    )
    .await;
    assert_eq!(foreign.status, 403);
    let huge = format!(r#"{{"version":"{}"}}"#, "1".repeat(4096));
    assert_eq!(request(&gw, "mbp", &huge).await.status, 413);

    // The real thing: queued to the agent's control socket as is.
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(r.status, 202);
    assert_eq!(r.json(), serde_json::json!({ "ok": true }));
    assert_eq!(
        recv(&mut agent.ctl).await,
        Message::Text(r#"{"t":"update","version":"1.6.0"}"#.into())
    );
    let metrics = String::from_utf8(get(gw.metrics, "/metrics", &[]).await.body).unwrap();
    assert!(
        metrics.contains("\nrd_agent_updates_requested_total 1\n"),
        "{metrics}"
    );
    let audit: Vec<_> = logs
        .lines()
        .into_iter()
        .filter(|l| l["target"] == "audit" && l["event"] == "agent_update")
        .collect();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["user"], EMAIL);
    assert_eq!(audit[0]["machine"], "mbp");
    assert_eq!(audit[0]["from"], "0.1.0");
    assert_eq!(audit[0]["to"], "1.6.0");
    assert!(audit[0]["ip"].is_string());

    // While it installs, a second request is refused; the status outlives the agent's
    // disconnect (the restart), and a new registration starts without one.
    let installing = UpdateStatus {
        phase: UpdatePhase::Installing,
        ..installable("1.6.0")
    };
    report(&gw, &mut agent, installing).await;
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "an update is already in progress")
    );
    drop(agent);
    for _ in 0..100 {
        if listed(&gw, "mbp").await["online"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "the agent is offline")
    );
    assert_eq!(listed_update(&gw, "mbp").await["phase"], "installing");
    let _agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    assert!(listed_update(&gw, "mbp").await.is_null());
    assert!(
        logs.lines()
            .iter()
            .any(|l| l["message"] == "agent update phase" && l["phase"] == "installing"),
        "phase change logged"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_cannot_install_says_why() {
    let tokens = TokenDir::new("updates");
    tokens.add("mbp", TOKEN);
    let gw = TestGateway::start(config(&tokens)).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let by_hand = UpdateStatus {
        can_install: false,
        cannot_install: "not installed with the installer".into(),
        ..installable("1.6.0")
    };
    report(&gw, &mut agent, by_hand).await;
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "not installed with the installer")
    );
    let up_to_date = UpdateStatus {
        can_install: true,
        checked_ms: Some(1_700_000_000_000),
        ..Default::default()
    };
    report(&gw, &mut agent, up_to_date).await;
    let r = request(&gw, "mbp", r#"{"version":"1.6.0"}"#).await;
    assert_eq!(
        (r.status, error(&r).as_str()),
        (409, "no update is available")
    );
    gw.shutdown().await;
}
