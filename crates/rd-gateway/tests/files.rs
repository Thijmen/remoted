//! File channels (docs/files.md): join, relay, refusals, limits, and that they leave screen
//! sessions alone (dev mode, fast timers).

mod common;

use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use rd_gateway::Config;
use rd_proto::bin::{FileAck, FileDataHeader, file_flags};
use rd_proto::close;
use rd_proto::control::GatewayToAgent;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "rd1.mbp.dev";
const FILES: &[&str] = &["files"];

fn config_with(f: impl FnOnce(&mut Config)) -> Config {
    let mut cfg = dev_config();
    f(&mut cfg);
    cfg
}

async fn machine_json(gw: &TestGateway, machine: &str) -> serde_json::Value {
    for _ in 0..100 {
        let r = get(gw.viewer, "/api/machines", &[]).await;
        assert_eq!(r.status, 200);
        if let Some(m) = r
            .json()
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["machine"] == machine && m["online"] == true)
        {
            return m.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{machine} never came online");
}

async fn send(ws: &mut Ws, msg: Message) {
    ws.send(msg).await.unwrap();
}

#[tokio::test]
async fn relays_text_and_file_frames_both_ways() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    assert_eq!(machine_json(&gw, "mbp").await["files"], true);
    let (mut viewer, mut data, _channel) = open_files(&gw, &mut agent, "mbp", &[]).await;

    let hello = r#"{"t":"hello","proto":1}"#;
    send(&mut viewer, Message::Text(hello.into())).await;
    assert_eq!(recv(&mut data).await, Message::Text(hello.into()));

    // An upload chunk viewer → agent, its ack back.
    let chunk = FileDataHeader {
        flags: file_flags::END,
        id: 9,
        seq: 1,
        offset: 0,
    }
    .to_message(&[7; 256 << 10]);
    send(&mut viewer, Message::Binary(chunk.clone().into())).await;
    assert_eq!(recv(&mut data).await, Message::Binary(chunk.into()));
    let ack = FileAck { seq: 1 }.to_message();
    send(&mut data, Message::Binary(ack.clone().into())).await;
    assert_eq!(recv(&mut viewer).await, Message::Binary(ack.into()));

    // A long listing (over 1 MiB) agent → viewer.
    let listing = format!(r#"{{"t":"list","id":1,"pad":"{}"}}"#, "x".repeat(3 << 20));
    send(&mut data, Message::Text(listing.clone().into())).await;
    assert_eq!(recv(&mut viewer).await, Message::Text(listing.into()));

    // The viewer hangs up: the agent's socket is closed too, and the slot is free again.
    viewer.close(None).await.unwrap();
    assert_eq!(expect_close(&mut data).await, 1000);
    let (_viewer, _data, _) = open_files(&gw, &mut agent, "mbp", &[]).await;
}

#[tokio::test]
async fn agents_without_the_capability_get_no_request_4011() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    assert_eq!(machine_json(&gw, "mbp").await["files"], false);
    let mut viewer = connect(&gw.viewer_url("/ws/files/mbp"), &[]).await.unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::UNSUPPORTED);
    // Nothing was sent to the agent: the next control message is a session request.
    let _v = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    assert!(matches!(
        agent.next_ctl().await,
        GatewayToAgent::SessionRequest { .. }
    ));
}

#[tokio::test]
async fn unknown_offline_and_invalid_machines_4003() {
    let gw = TestGateway::start(dev_config()).await;
    for path in ["/ws/files/nope", "/ws/files/bad%20name"] {
        let mut viewer = connect(&gw.viewer_url(path), &[]).await.unwrap();
        assert_eq!(
            expect_close(&mut viewer).await,
            close::AGENT_OFFLINE,
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_fifth_channel_is_refused_1013() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let mut open = Vec::new();
    for _ in 0..4 {
        open.push(open_files(&gw, &mut agent, "mbp", &[]).await);
    }
    let mut fifth = connect(&gw.viewer_url("/ws/files/mbp"), &[]).await.unwrap();
    assert_eq!(expect_close(&mut fifth).await, close::TRY_AGAIN_LATER);
    // Closing one frees a slot.
    let (mut viewer, mut data, _) = open.pop().unwrap();
    viewer.close(None).await.unwrap();
    expect_close(&mut data).await;
    let _again = open_files(&gw, &mut agent, "mbp", &[]).await;
}

#[tokio::test]
async fn agent_not_dialing_4004_and_late_dial_404() {
    let gw = TestGateway::start(config_with(|c| {
        c.timeouts.session_request = Duration::from_millis(300);
    }))
    .await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let mut viewer = connect(&gw.viewer_url("/ws/files/mbp"), &[]).await.unwrap();
    let channel = agent.expect_files_request().await;
    assert_eq!(expect_close(&mut viewer).await, close::AGENT_TIMEOUT);
    assert_eq!(refused_status(agent.dial_files(&channel).await), 404);
    // A file channel id is no session id, and the other way round.
    assert_eq!(refused_status(agent.dial(&channel).await), 404);
}

#[tokio::test]
async fn files_and_the_screen_session_do_not_end_each_other() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let (mut sviewer, mut sdata, _) = open_session(&gw, &mut agent, "mbp", &[]).await;
    let (mut fviewer, mut fdata, _) = open_files(&gw, &mut agent, "mbp", &[]).await;

    // Both relay.
    send(&mut sdata, Message::Text(r#"{"t":"stats"}"#.into())).await;
    assert_eq!(
        recv(&mut sviewer).await,
        Message::Text(r#"{"t":"stats"}"#.into())
    );
    send(&mut fdata, Message::Text(r#"{"t":"ok","id":1}"#.into())).await;
    assert_eq!(
        recv(&mut fviewer).await,
        Message::Text(r#"{"t":"ok","id":1}"#.into())
    );

    // Ending the session leaves the file channel open.
    sviewer.close(None).await.unwrap();
    expect_close(&mut sdata).await;
    send(&mut fdata, Message::Text(r#"{"t":"ok","id":2}"#.into())).await;
    assert_eq!(
        recv(&mut fviewer).await,
        Message::Text(r#"{"t":"ok","id":2}"#.into())
    );
    // And a new session (which also ends the old one's agent side) leaves it open too.
    let (_v2, _d2, _) = {
        // The previous session's session_end comes first.
        match agent.next_ctl().await {
            GatewayToAgent::SessionEnd { .. } => {}
            other => panic!("expected session_end, got {other:?}"),
        }
        open_session(&gw, &mut agent, "mbp", &[]).await
    };
    send(&mut fviewer, Message::Text(r#"{"t":"list","id":3}"#.into())).await;
    assert_eq!(
        recv(&mut fdata).await,
        Message::Text(r#"{"t":"list","id":3}"#.into())
    );
}

#[tokio::test]
async fn idle_channels_close_4008_and_any_message_counts() {
    let gw = TestGateway::start(config_with(|c| {
        c.timeouts.idle = Duration::from_millis(400);
    }))
    .await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let (mut viewer, mut data, _) = open_files(&gw, &mut agent, "mbp", &[]).await;
    // Only agent → viewer traffic (a long download) keeps the channel alive.
    for i in 0..10 {
        let frame = FileDataHeader {
            flags: 0,
            id: 1,
            seq: i + 1,
            offset: u64::from(i) * 4,
        }
        .to_message(&[1, 2, 3, 4]);
        send(&mut data, Message::Binary(frame.into())).await;
        assert!(matches!(recv(&mut viewer).await, Message::Binary(_)));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Then silence.
    assert_eq!(expect_close(&mut viewer).await, close::IDLE_TIMEOUT);
    assert_eq!(expect_close(&mut data).await, 1000);
}

#[tokio::test]
async fn revoking_the_device_ends_its_channels_4003() {
    // A managed device (added in the portal) can be removed; its channels end.
    let gw = TestGateway::start(dev_config()).await;
    let added = http(
        gw.viewer,
        "POST",
        "/api/devices",
        &[("content-type", "application/json")],
        br#"{"machine":"studio","name":"Studio"}"#.to_vec(),
    )
    .await;
    assert_eq!(added.status, 201);
    let token = added.json()["token"].as_str().unwrap().to_owned();
    let mut agent = FakeAgent::connect_with_caps(&gw, "studio", &token, FILES)
        .await
        .unwrap();
    let (mut viewer, _data, _) = open_files(&gw, &mut agent, "studio", &[]).await;
    let removed = http(gw.viewer, "DELETE", "/api/devices/studio", &[], Vec::new()).await;
    assert_eq!(removed.status, 204);
    assert_eq!(expect_close(&mut viewer).await, close::AGENT_OFFLINE);
}

#[tokio::test]
async fn shutdown_closes_file_channels_with_1012() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let (mut viewer, mut data, _) = open_files(&gw, &mut agent, "mbp", &[]).await;
    let closing = tokio::spawn(async move {
        (
            expect_close(&mut viewer).await,
            expect_close(&mut data).await,
        )
    });
    gw.shutdown().await;
    assert_eq!(closing.await.unwrap(), (1012, 1012));
}

#[tokio::test]
async fn metrics_count_file_channels() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, FILES)
        .await
        .unwrap();
    let (mut viewer, mut data, _) = open_files(&gw, &mut agent, "mbp", &[]).await;
    let text = String::from_utf8(get(gw.metrics, "/metrics", &[]).await.body).unwrap();
    assert!(text.contains("rd_files_channels_active 1"), "{text}");
    assert!(text.contains("rd_files_channels_started_total 1"), "{text}");
    send(
        &mut viewer,
        Message::Binary(vec![0x41, 0, 0, 0, 1, 0, 0, 0].into()),
    )
    .await;
    recv(&mut data).await;
    viewer.close(None).await.unwrap();
    expect_close(&mut data).await;
    for _ in 0..50 {
        let text = String::from_utf8(get(gw.metrics, "/metrics", &[]).await.body).unwrap();
        if text.contains("rd_files_channels_active 0") {
            assert!(text.contains("rd_files_bytes_up_total 8"), "{text}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the file channel never ended");
}
