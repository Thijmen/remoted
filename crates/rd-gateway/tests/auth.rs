//! Agent and viewer authentication in production (non-dev) mode.

mod common;

use common::*;
use rd_gateway::Config;

const ORIGIN: &str = "https://rd.example.test";
const EMAIL: &str = "me@example.test";
const TOKEN: &str = "rd1.mbp.0123456789abcdef0123456789abcdef";

fn prod_config(tokens: &TokenDir) -> Config {
    Config {
        dev: false,
        public_origin: Some(ORIGIN.into()),
        allowed_emails: [EMAIL.to_owned()].into_iter().collect(),
        agent_tokens_dir: tokens.path.clone(),
        ..dev_config()
    }
}

async fn agent_status(gw: &TestGateway, path: &str, headers: &[(&str, &str)]) -> u16 {
    refused_status(connect(&gw.agent_url(path), headers).await)
}

#[tokio::test]
async fn agent_auth_401_and_rate_limit() {
    let tokens = TokenDir::new("auth");
    tokens.add("mbp", TOKEN);
    let gw = TestGateway::start(prod_config(&tokens)).await;

    assert_eq!(agent_status(&gw, "/ws/agent", &[]).await, 401);
    // Plain HTTP (no upgrade) is authenticated first, too.
    assert_eq!(get(gw.agent, "/ws/agent", &[]).await.status, 401);
    assert_eq!(get(gw.agent, "/", &[]).await.status, 404);
    assert_eq!(get(gw.agent, "/api/machines", &[]).await.status, 404);

    // Failures are counted per rightmost X-Forwarded-For entry.
    let bad = "Bearer rd1.mbp.wrong";
    let xff = "203.0.113.9, 198.51.100.7";
    for attempt in 0..4 {
        let status = agent_status(
            &gw,
            "/ws/agent",
            &[("authorization", bad), ("x-forwarded-for", xff)],
        )
        .await;
        assert_eq!(status, 401, "attempt {attempt}");
    }
    // Unknown machine and malformed tokens count as failures as well (5th failure).
    let status = agent_status(
        &gw,
        "/ws/agent",
        &[
            ("authorization", "Bearer rd1.nope.secret"),
            ("x-forwarded-for", xff),
        ],
    )
    .await;
    assert_eq!(status, 401);
    let status = agent_status(
        &gw,
        "/ws/agent",
        &[("authorization", bad), ("x-forwarded-for", xff)],
    )
    .await;
    assert_eq!(status, 429, "6th failure within a minute is rate limited");
    // While limited, even the right token is refused for that client...
    let good = format!("Bearer {TOKEN}");
    let status = agent_status(
        &gw,
        "/ws/agent",
        &[
            ("authorization", &good),
            ("x-forwarded-for", "198.51.100.7"),
        ],
    )
    .await;
    assert_eq!(status, 429);
    // ...but other clients (the left XFF entry is client-controlled and ignored) are fine.
    let ok = connect(
        &gw.agent_url("/ws/agent"),
        &[
            ("authorization", &good),
            ("x-forwarded-for", "198.51.100.7, 192.0.2.50"),
        ],
    )
    .await;
    assert!(ok.is_ok(), "valid token from another client accepted");
    drop(ok);

    // The session socket needs the token as well, and an existing pending session.
    let status = agent_status(
        &gw,
        "/ws/agent/session/00112233445566778899aabbccddeeff",
        &[],
    )
    .await;
    assert_eq!(status, 401);
    let status = agent_status(
        &gw,
        "/ws/agent/session/00112233445566778899aabbccddeeff",
        &[("authorization", &good)],
    )
    .await;
    assert_eq!(status, 404);

    let metrics = get(gw.metrics, "/metrics", &[]).await;
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(text.contains("rd_agent_auth_failures_total 10"), "{text}");
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_register_must_match_token_machine() {
    let tokens = TokenDir::new("match");
    tokens.add("mbp", TOKEN);
    let gw = TestGateway::start(prod_config(&tokens)).await;
    let mut agent = FakeAgent::connect_unregistered(&gw, TOKEN).await.unwrap();
    agent.register("other").await;
    assert_eq!(expect_close(&mut agent.ctl).await, 4002);
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    // Graceful shutdown tells connected agents to reconnect (1012 service restart).
    let shutdown = tokio::spawn(gw.shutdown());
    assert_eq!(expect_close(&mut agent.ctl).await, 1012);
    shutdown.await.unwrap();
}

fn assert_security_headers(r: &HttpResponse) {
    assert_eq!(
        r.header("content-security-policy"),
        Some(
            "default-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'"
        )
    );
    assert_eq!(r.header("cross-origin-opener-policy"), Some("same-origin"));
    assert_eq!(
        r.header("cross-origin-embedder-policy"),
        Some("require-corp")
    );
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
}

#[tokio::test]
async fn viewer_403_without_allowlisted_email_or_origin() {
    let tokens = TokenDir::new("viewer");
    let gw = TestGateway::start(prod_config(&tokens)).await;

    let r = get(gw.viewer, "/api/me", &[]).await;
    assert_eq!(r.status, 403);
    assert_security_headers(&r);
    let r = get(
        gw.viewer,
        "/",
        &[("x-authentik-email", "intruder@example.test")],
    )
    .await;
    assert_eq!(r.status, 403);

    let r = get(
        gw.viewer,
        "/api/me",
        &[("x-authentik-email", "ME@Example.Test")],
    )
    .await;
    assert_eq!(r.status, 200);
    assert_security_headers(&r);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(
        r.json(),
        serde_json::json!({ "email": "me@example.test", "uid": null, "dev": false })
    );

    // WebSocket upgrades also need the exact public Origin.
    let url = gw.viewer_url("/ws/view/mbp");
    let auth = ("x-authentik-email", EMAIL);
    assert_eq!(refused_status(connect(&url, &[]).await), 403);
    assert_eq!(refused_status(connect(&url, &[auth]).await), 403);
    let foreign = ("origin", "https://evil.example.test");
    assert_eq!(refused_status(connect(&url, &[auth, foreign]).await), 403);
    let probe = gw.viewer_url("/ws/probe");
    assert_eq!(refused_status(connect(&probe, &[auth, foreign]).await), 403);
    let mut viewer = connect(&url, &[auth, ("origin", ORIGIN)]).await.unwrap();
    assert_eq!(
        expect_close(&mut viewer).await,
        4003,
        "allowed, but agent offline"
    );

    // Cross-origin POSTs are refused; same-origin ones pass.
    let report = br#"{"ok":true}"#.to_vec();
    let r = http(
        gw.viewer,
        "POST",
        "/api/probe/report",
        &[auth, foreign],
        report.clone(),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = http(
        gw.viewer,
        "POST",
        "/api/probe/report",
        &[auth, ("origin", ORIGIN)],
        report,
    )
    .await;
    assert_eq!(r.status, 204);

    let metrics = get(gw.metrics, "/metrics", &[]).await;
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(text.contains("rd_viewer_auth_failures_total 7"), "{text}");
    gw.shutdown().await;
}

#[tokio::test]
async fn viewer_uid_allowlist_is_checked_when_set() {
    let tokens = TokenDir::new("uid");
    let mut cfg = prod_config(&tokens);
    cfg.allowed_uids = ["uid-owner".to_owned()].into();
    let gw = TestGateway::start(cfg).await;
    let email = ("x-authentik-email", EMAIL);

    // The right email alone is not enough once uids are configured.
    assert_eq!(get(gw.viewer, "/api/me", &[email]).await.status, 403);
    let other = ("x-authentik-uid", "uid-someone-else");
    assert_eq!(get(gw.viewer, "/api/me", &[email, other]).await.status, 403);

    let owner = ("x-authentik-uid", "uid-owner");
    let r = get(gw.viewer, "/api/me", &[email, owner]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["uid"], "uid-owner");
    gw.shutdown().await;
}
