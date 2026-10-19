//! Pages, hashed assets, caching headers, metrics/health, and RD_CHAOS on the relay.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use rd_gateway::{Chaos, Config};
use tokio_tungstenite::tungstenite::Message;

struct AssetDir(PathBuf);

impl AssetDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rd-gw-test-assets-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        for page in ["index", "viewer", "probe", "latency"] {
            std::fs::write(
                dir.join(format!("{page}.html")),
                format!("<!doctype html>{page}"),
            )
            .unwrap();
        }
        std::fs::write(dir.join("assets/app-1a2b3c.js"), "console.log(1)").unwrap();
        std::fs::write(dir.join("favicon.svg"), "<svg/>").unwrap();
        AssetDir(dir)
    }
}

impl Drop for AssetDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn pages_assets_and_cache_headers() {
    let assets = AssetDir::new();
    let gw = TestGateway::start(Config {
        assets_dir: Some(assets.0.clone()),
        ..dev_config()
    })
    .await;

    for (path, body) in [
        ("/", "index"),
        ("/m/mbp", "viewer"),
        ("/probe", "probe"),
        ("/latency-target", "latency"),
    ] {
        let r = get(gw.viewer, path, &[]).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(r.header("cache-control"), Some("no-cache"));
        assert_eq!(
            r.header("cross-origin-embedder-policy"),
            Some("require-corp")
        );
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            format!("<!doctype html>{body}")
        );
    }

    // keytest.html is missing from this build: placeholder page instead of a bare 404.
    let r = get(gw.viewer, "/keytest", &[]).await;
    assert_eq!(r.status, 503);
    assert!(String::from_utf8(r.body).unwrap().contains("keytest.html"));

    let r = get(gw.viewer, "/assets/app-1a2b3c.js", &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(
        r.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(get(gw.viewer, "/assets/nope.js", &[]).await.status, 404);
    assert_eq!(
        get(gw.viewer, "/assets/../index.html", &[]).await.status,
        404
    );
    assert_eq!(
        get(gw.viewer, "/assets/%2e%2e/index.html", &[])
            .await
            .status,
        404
    );

    let r = get(gw.viewer, "/favicon.svg", &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(get(gw.viewer, "/nope", &[]).await.status, 404);

    let r = get(gw.viewer, "/api/me", &[]).await;
    assert_eq!(
        r.json(),
        serde_json::json!({ "email": "dev@localhost", "uid": null, "dev": true, "devices": true })
    );

    let r = get(gw.metrics, "/healthz", &[]).await;
    assert_eq!((r.status, &r.body[..]), (200, &b"ok"[..]));
    let r = get(gw.metrics, "/metrics", &[]).await;
    assert_eq!(r.status, 200);
    let text = String::from_utf8(r.body).unwrap();
    assert!(
        text.contains("# TYPE rd_agents_online gauge\nrd_agents_online 0"),
        "{text}"
    );
    assert_eq!(get(gw.metrics, "/", &[]).await.status, 404);
    gw.shutdown().await;
}

#[tokio::test]
async fn chaos_delays_and_paces_agent_to_viewer_in_order() {
    // 800 kbps = 100 kB/s; 10 × 2 kB = 20 kB takes ≥ 200 ms on the link plus 150 ms delay.
    let gw = TestGateway::start(Config {
        chaos: Some(Chaos {
            bw_kbps: Some(800),
            delay_ms: 150,
        }),
        ..dev_config()
    })
    .await;
    let mut agent = FakeAgent::connect(&gw, "mbp", "rd1.mbp.dev").await.unwrap();
    let (mut viewer, mut data, _) = open_session(&gw, &mut agent, "mbp", &[]).await;

    let start = tokio::time::Instant::now();
    for i in 0..10u8 {
        data.send(Message::Binary(vec![i; 2000].into()))
            .await
            .unwrap();
    }
    let mut first_at = None;
    for i in 0..10u8 {
        assert_eq!(
            recv(&mut viewer).await,
            Message::Binary(vec![i; 2000].into())
        );
        first_at.get_or_insert(start.elapsed());
    }
    let first_at = first_at.unwrap();
    let total = start.elapsed();
    assert!(
        first_at >= Duration::from_millis(150),
        "first after {first_at:?}"
    );
    assert!(total >= Duration::from_millis(340), "all after {total:?}");
    assert!(total < Duration::from_secs(2), "all after {total:?}");

    // viewer→agent is not affected.
    let t = tokio::time::Instant::now();
    viewer.send(Message::Text("up".into())).await.unwrap();
    assert_eq!(recv(&mut data).await, Message::Text("up".into()));
    assert!(t.elapsed() < Duration::from_millis(100));
    drop((viewer, data, agent));
    gw.shutdown().await;
}
