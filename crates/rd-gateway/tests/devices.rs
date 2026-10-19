//! Devices added from the portal: the API, agent auth with issued tokens, revocation and the
//! store surviving a restart (production mode).

mod common;

use common::*;
use rd_gateway::Config;
use rd_proto::close;
use rd_proto::setup_code::SetupCode;

const ORIGIN: &str = "https://rd.example.test";
const EMAIL: &str = "me@example.test";
const AGENT_URL: &str = "wss://rd-agent.example.test";
const VIEWER: (&str, &str) = ("x-authentik-email", EMAIL);
const JSON: (&str, &str) = ("content-type", "application/json");

fn config(tokens: &TokenDir, state: &TokenDir) -> Config {
    Config {
        dev: false,
        public_origin: Some(ORIGIN.into()),
        allowed_emails: [EMAIL.to_owned()].into_iter().collect(),
        agent_tokens_dir: tokens.path.clone(),
        state_dir: Some(state.path.clone()),
        agent_public_url: Some(AGENT_URL.into()),
        ..dev_config()
    }
}

async fn add(gw: &TestGateway, body: &str) -> HttpResponse {
    http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[VIEWER, JSON, ("origin", ORIGIN)],
        body.into(),
    )
    .await
}

fn machines(r: &HttpResponse) -> Vec<serde_json::Value> {
    r.json().as_array().unwrap().clone()
}

async fn agent_refused(gw: &TestGateway, token: &str) -> u16 {
    let auth = format!("Bearer {token}");
    refused_status(connect(&gw.agent_url("/ws/agent"), &[("authorization", &auth)]).await)
}

#[tokio::test]
async fn add_connect_replace_remove() {
    let tokens = TokenDir::new("dev-tokens");
    let state = TokenDir::new("dev-state");
    let gw = TestGateway::start(config(&tokens, &state)).await;

    let me = get(gw.viewer, "/api/me", &[VIEWER]).await.json();
    assert_eq!(me["devices"], true);

    // Add: the setup code carries the agent URL and a token for the machine.
    let r = add(&gw, r#"{"machine":"studio","name":"Mac Studio"}"#).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let body = r.json();
    let code = SetupCode::decode(body["setup_code"].as_str().unwrap()).unwrap();
    assert_eq!(code.gateway, AGENT_URL);
    assert_eq!(code.machine(), "studio");
    assert_eq!(code.token, body["token"].as_str().unwrap());
    let token = code.token.clone();

    // It shows in the portal while offline, under its name, as managed.
    let list = machines(&get(gw.viewer, "/api/machines", &[VIEWER]).await);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["machine"], "studio");
    assert_eq!(list[0]["name"], "Mac Studio");
    assert_eq!(list[0]["managed"], true);
    assert_eq!(list[0]["online"], false);

    // The agent connects with the issued token.
    let mut agent = FakeAgent::connect(&gw, "studio", &token).await.unwrap();
    let list = machines(&get(gw.viewer, "/api/machines", &[VIEWER]).await);
    assert_eq!(list[0]["online"], true);

    // A new token: the connected agent is closed with 4002, the old token stops working.
    let r = http(
        gw.viewer,
        "POST",
        "/api/devices/studio/token",
        &[VIEWER, JSON],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(r.status, 200);
    let new_token = SetupCode::decode(r.json()["setup_code"].as_str().unwrap())
        .unwrap()
        .token;
    assert_ne!(new_token, token);
    assert_eq!(expect_close(&mut agent.ctl).await, close::FORBIDDEN);
    assert_eq!(agent_refused(&gw, &token).await, 401);
    let mut agent = FakeAgent::connect(&gw, "studio", &new_token).await.unwrap();

    // Remove: closed again, the token is refused and the device leaves the portal.
    let r = http(
        gw.viewer,
        "DELETE",
        "/api/devices/studio",
        &[VIEWER],
        vec![],
    )
    .await;
    assert_eq!(r.status, 204);
    assert_eq!(expect_close(&mut agent.ctl).await, close::FORBIDDEN);
    assert_eq!(agent_refused(&gw, &new_token).await, 401);
    assert!(machines(&get(gw.viewer, "/api/machines", &[VIEWER]).await).is_empty());
    let r = http(
        gw.viewer,
        "DELETE",
        "/api/devices/studio",
        &[VIEWER],
        vec![],
    )
    .await;
    assert_eq!(r.status, 404);
    gw.shutdown().await;
}

#[tokio::test]
async fn refuses_bad_requests() {
    let tokens = TokenDir::new("dev-tokens");
    tokens.add("mbp", "rd1.mbp.provisioned");
    let state = TokenDir::new("dev-state");
    let gw = TestGateway::start(config(&tokens, &state)).await;

    assert_eq!(add(&gw, r#"{"machine":"a.b"}"#).await.status, 400);
    assert_eq!(
        add(&gw, r#"{"machine":"pc","name":"x\ny"}"#).await.status,
        400
    );
    // A machine with a provisioned token cannot become a device.
    assert_eq!(add(&gw, r#"{"machine":"mbp"}"#).await.status, 409);
    assert_eq!(add(&gw, r#"{"machine":"pc"}"#).await.status, 201);
    assert_eq!(add(&gw, r#"{"machine":"pc"}"#).await.status, 409);
    // Only JSON (a plain form post is refused before it is read).
    let form = http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[
            VIEWER,
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        b"machine=x".to_vec(),
    )
    .await;
    assert_eq!(form.status, 415);
    // Cross-site and unauthenticated requests are refused by the viewer auth.
    let foreign = http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[VIEWER, JSON, ("origin", "https://evil.example.test")],
        br#"{"machine":"evil"}"#.to_vec(),
    )
    .await;
    assert_eq!(foreign.status, 403);
    let anonymous = http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[JSON],
        br#"{"machine":"anon"}"#.to_vec(),
    )
    .await;
    assert_eq!(anonymous.status, 403);
    let r = http(
        gw.viewer,
        "POST",
        "/api/devices/nope/token",
        &[VIEWER, JSON],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(r.status, 404);
    // The provisioned machine still works with its own token, and is not managed.
    let _agent = FakeAgent::connect(&gw, "mbp", "rd1.mbp.provisioned")
        .await
        .unwrap();
    let list = machines(&get(gw.viewer, "/api/machines", &[VIEWER]).await);
    let mbp = list.iter().find(|m| m["machine"] == "mbp").unwrap();
    assert_eq!(mbp["managed"], false);
    gw.shutdown().await;
}

#[tokio::test]
async fn devices_survive_a_restart() {
    let tokens = TokenDir::new("dev-tokens");
    let state = TokenDir::new("dev-state");
    let gw = TestGateway::start(config(&tokens, &state)).await;
    let r = add(&gw, r#"{"machine":"pc","name":"Windows PC"}"#).await;
    let token = r.json()["token"].as_str().unwrap().to_owned();
    gw.shutdown().await;

    let gw = TestGateway::start(config(&tokens, &state)).await;
    let list = machines(&get(gw.viewer, "/api/machines", &[VIEWER]).await);
    assert_eq!(list[0]["name"], "Windows PC");
    let _agent = FakeAgent::connect(&gw, "pc", &token).await.unwrap();
    gw.shutdown().await;
}

#[tokio::test]
async fn off_without_a_state_dir_or_agent_url() {
    let tokens = TokenDir::new("dev-tokens");
    let state = TokenDir::new("dev-state");
    for cfg in [
        Config {
            state_dir: None,
            ..config(&tokens, &state)
        },
        Config {
            agent_public_url: None,
            ..config(&tokens, &state)
        },
    ] {
        let gw = TestGateway::start(cfg).await;
        assert_eq!(
            get(gw.viewer, "/api/me", &[VIEWER]).await.json()["devices"],
            false
        );
        assert_eq!(add(&gw, r#"{"machine":"pc"}"#).await.status, 503);
        gw.shutdown().await;
    }
}

#[tokio::test]
async fn dev_mode_keeps_devices_in_memory_with_the_local_agent_url() {
    let gw = TestGateway::start(dev_config()).await;
    let r = http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[JSON],
        br#"{"machine":"pc"}"#.to_vec(),
    )
    .await;
    assert_eq!(r.status, 201);
    let code = SetupCode::decode(r.json()["setup_code"].as_str().unwrap()).unwrap();
    assert_eq!(code.gateway, format!("ws://{}", gw.agent));
    gw.shutdown().await;
}
