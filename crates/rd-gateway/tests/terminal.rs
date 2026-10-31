//! Terminal channels (docs/terminal.md): join, relay, refusals and limits, and that they leave
//! screen sessions and file channels alone (dev mode, fast timers).

mod common;

use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use rd_proto::bin::{TermAck, TermOutHeader, term_in_message};
use rd_proto::close;
use rd_proto::control::GatewayToAgent;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "rd1.mbp.dev";
const TERMINAL: &[&str] = &["terminal"];

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
async fn relays_terminal_frames_both_ways() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, TERMINAL)
        .await
        .unwrap();
    let m = machine_json(&gw, "mbp").await;
    assert_eq!(m["terminal"], true);
    assert_eq!(m["files"], false);
    let (mut viewer, mut data, _channel) = open_terminal(&gw, &mut agent, "mbp", &[]).await;

    let hello = r#"{"t":"hello","proto":1,"cols":80,"rows":24}"#;
    send(&mut viewer, Message::Text(hello.into())).await;
    assert_eq!(recv(&mut data).await, Message::Text(hello.into()));
    let input = term_in_message(b"ls\r");
    send(&mut viewer, Message::Binary(input.clone().into())).await;
    assert_eq!(recv(&mut data).await, Message::Binary(input.into()));
    let out = TermOutHeader { seq: 1, offset: 0 }.to_message(&[b'x'; 64 << 10]);
    send(&mut data, Message::Binary(out.clone().into())).await;
    assert_eq!(recv(&mut viewer).await, Message::Binary(out.into()));
    let ack = TermAck { seq: 1 }.to_message();
    send(&mut viewer, Message::Binary(ack.clone().into())).await;
    assert_eq!(recv(&mut data).await, Message::Binary(ack.into()));

    // A take-over close code chosen by the agent reaches the viewer as is.
    data.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: close::KICKED.into(),
        reason: "taken over".into(),
    }))
    .await
    .unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::KICKED);
    // The slot is free again.
    let (_viewer, _data, _) = open_terminal(&gw, &mut agent, "mbp", &[]).await;
}

#[tokio::test]
async fn agents_without_the_capability_get_no_request_4011() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, &["files"])
        .await
        .unwrap();
    assert_eq!(machine_json(&gw, "mbp").await["terminal"], false);
    let mut viewer = connect(&gw.viewer_url("/ws/terminal/mbp"), &[])
        .await
        .unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::UNSUPPORTED);
    // Nothing was sent to the agent: the next control message is a file request.
    let _f = connect(&gw.viewer_url("/ws/files/mbp"), &[]).await.unwrap();
    assert!(matches!(
        agent.next_ctl().await,
        GatewayToAgent::FilesRequest { .. }
    ));
}

#[tokio::test]
async fn unknown_machines_4003() {
    let gw = TestGateway::start(dev_config()).await;
    let mut viewer = connect(&gw.viewer_url("/ws/terminal/nope"), &[])
        .await
        .unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::AGENT_OFFLINE);
}

#[tokio::test]
async fn a_ninth_terminal_is_refused_1013_but_files_still_open() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, &["files", "terminal"])
        .await
        .unwrap();
    let mut open = Vec::new();
    for _ in 0..8 {
        open.push(open_terminal(&gw, &mut agent, "mbp", &[]).await);
    }
    let mut ninth = connect(&gw.viewer_url("/ws/terminal/mbp"), &[])
        .await
        .unwrap();
    assert_eq!(expect_close(&mut ninth).await, close::TRY_AGAIN_LATER);
    // Terminals do not count against file channels.
    let _files = open_files(&gw, &mut agent, "mbp", &[]).await;
    // Closing one frees a slot.
    let (mut viewer, mut data, _) = open.pop().unwrap();
    viewer.close(None).await.unwrap();
    expect_close(&mut data).await;
    let _again = open_terminal(&gw, &mut agent, "mbp", &[]).await;
}

#[tokio::test]
async fn terminal_and_file_channel_ids_are_not_interchangeable() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, &["files", "terminal"])
        .await
        .unwrap();
    let _viewer = connect(&gw.viewer_url("/ws/terminal/mbp"), &[])
        .await
        .unwrap();
    let channel = agent.expect_terminal_request().await;
    assert_eq!(refused_status(agent.dial_files(&channel).await), 404);
    assert_eq!(refused_status(agent.dial(&channel).await), 404);
    assert!(agent.dial_terminal(&channel).await.is_ok());
}

#[tokio::test]
async fn the_terminal_page_is_served() {
    let gw = TestGateway::start(dev_config()).await;
    let r = get(gw.viewer, "/t/mbp", &[]).await;
    // The test gateway may run without a viewer build; either way the route exists.
    assert!(r.status == 200 || r.status == 503, "status {}", r.status);
}

#[tokio::test]
async fn headless_agents_serve_the_terminal_but_no_screen_4011() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent =
        FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, &["files", "terminal", "headless"])
            .await
            .unwrap();
    let m = machine_json(&gw, "mbp").await;
    assert_eq!(m["screen"], false);
    assert_eq!(m["terminal"], true);
    // No session_request reaches the agent: the gateway refuses the screen itself.
    let mut viewer = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::UNSUPPORTED);
    // The terminal works as usual.
    let (_viewer, _data, _channel) = open_terminal(&gw, &mut agent, "mbp", &[]).await;
}

#[tokio::test]
async fn agents_have_a_screen_unless_they_say_headless() {
    let gw = TestGateway::start(dev_config()).await;
    let _agent = FakeAgent::connect_with_caps(&gw, "mbp", TOKEN, TERMINAL)
        .await
        .unwrap();
    assert_eq!(machine_json(&gw, "mbp").await["screen"], true);
}
