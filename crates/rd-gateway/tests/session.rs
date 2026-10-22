//! Registration, session join/relay and every session end path (dev mode, fast timers).

mod common;

use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use rd_gateway::{Config, MachineConfig, RecoveryLink};
use rd_proto::bin::{InputEvent, VideoAck, VideoHeader};
use rd_proto::close;
use rd_proto::control::{AgentToGateway, GatewayToAgent};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

const TOKEN: &str = "rd1.mbp.dev";

fn config_with(f: impl FnOnce(&mut Config)) -> Config {
    let mut cfg = dev_config();
    f(&mut cfg);
    cfg
}

async fn machines(gw: &TestGateway) -> serde_json::Value {
    let r = get(gw.viewer, "/api/machines", &[]).await;
    assert_eq!(r.status, 200);
    r.json()
}

/// Poll the portal until `machine`'s `online` flag matches.
async fn wait_online(gw: &TestGateway, machine: &str, online: bool) -> serde_json::Value {
    for _ in 0..100 {
        let list = machines(gw).await;
        if let Some(m) = list
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["machine"] == machine)
            && m["online"] == online
        {
            return m.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{machine} never became online={online}");
}

#[tokio::test]
async fn registration_replacement_4009_and_portal_listing() {
    let gw = TestGateway::start(config_with(|c| {
        c.machines = vec![MachineConfig {
            machine: "pc".into(),
            name: Some("Office PC".into()),
            recovery: vec![RecoveryLink {
                label: "RDP".into(),
                url: "https://guac.example/pc".into(),
            }],
        }];
    }))
    .await;

    let list = machines(&gw).await;
    assert_eq!(
        list,
        serde_json::json!([{
            "machine": "pc", "name": "Office PC", "managed": false, "online": false, "since_ms": null,
            "os": null, "agent_version": null, "state": null, "viewer": null,
            "recovery": [{"label": "RDP", "url": "https://guac.example/pc"}], "update": null,
            "files": false, "terminal": false
        }])
    );

    let mut first = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let mut second = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    assert_eq!(expect_close(&mut first.ctl).await, 4009);

    let mbp = wait_online(&gw, "mbp", true).await;
    assert_eq!(mbp["name"], "mbp");
    assert_eq!(mbp["os"], "macOS 26.3 (test)");
    assert_eq!(mbp["agent_version"], "0.1.0");
    assert_eq!(
        mbp["state"],
        serde_json::json!({"locked": false, "perms": {"screen": false, "input": false}, "in_session": false, "secure": "unavailable", "disconnected": false})
    );
    assert!(mbp["since_ms"].as_u64().unwrap() > 1_600_000_000_000);
    assert_eq!(
        machines(&gw).await[0]["machine"],
        "pc",
        "configured machines first"
    );

    // State updates are reflected; the replacement is the live connection.
    send_json(
        &mut second.ctl,
        &AgentToGateway::State(rd_proto::control::AgentState {
            locked: true,
            ..Default::default()
        }),
    )
    .await;
    for _ in 0..100 {
        if wait_online(&gw, "mbp", true).await["state"]["locked"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(wait_online(&gw, "mbp", true).await["state"]["locked"], true);

    second.ctl.close(None).await.unwrap();
    let mbp = wait_online(&gw, "mbp", false).await;
    assert_eq!(
        mbp["os"], "macOS 26.3 (test)",
        "last known info kept while offline"
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn session_relays_byte_exact_both_ways() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;

    let listing = wait_online(&gw, "mbp", true).await;
    assert_eq!(listing["viewer"]["user"], "dev@localhost");

    // viewer → agent: odd JSON spacing must survive (no re-encoding), every byte value, empty.
    let all_bytes: Vec<u8> = (0..=255u8).collect();
    let up = vec![
        Message::Text(r#"{"t":"hello",  "proto":1,"min_proto":1 ,"x":"ü"}"#.into()),
        Message::Binary(
            InputEvent::Key {
                down: true,
                repeat: false,
                hid: 0x0007_0004,
            }
            .to_message()
            .into(),
        ),
        Message::Binary(all_bytes.clone().into()),
        Message::Binary(Vec::new().into()),
        Message::Binary(VideoAck::default().to_message().into()),
    ];
    for m in &up {
        viewer.send(m.clone()).await.unwrap();
    }
    for m in &up {
        assert_eq!(&recv(&mut data).await, m);
    }

    // agent → viewer, including a 1 MiB "keyframe" and a WS-level ping that must not cross.
    let big: Vec<u8> = (0..(1 << 20)).map(|i| (i * 7 % 251) as u8).collect();
    let frame = VideoHeader {
        flags: 5,
        epoch: 1,
        seq: 1,
        capture_us: 123,
        encode_us: 4,
    }
    .to_message(&big);
    let down = vec![
        Message::Text(r#"{"t":"video_config","epoch":1, "codec":"avc1.640033"}"#.into()),
        Message::Binary(frame.into()),
        Message::Binary(all_bytes.into()),
    ];
    data.send(Message::Ping(b"agent-ping".to_vec().into()))
        .await
        .unwrap();
    for m in &down {
        data.send(m.clone()).await.unwrap();
    }
    for m in &down {
        let got = loop {
            match tokio::time::timeout(WAIT, viewer.next()).await.unwrap() {
                Some(Ok(Message::Ping(p))) => {
                    assert_ne!(&p[..], b"agent-ping", "pings must not be forwarded")
                }
                Some(Ok(msg)) => break msg,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(&got, m);
    }

    // Viewer leaves: the agent's data socket is closed and the agent is told on control.
    viewer
        .close(Some(CloseFrame {
            code: 1000.into(),
            reason: "bye".into(),
        }))
        .await
        .unwrap();
    assert_eq!(expect_close(&mut data).await, 1000);
    assert_eq!(agent.expect_session_end(&session).await, "viewer_closed");
    let listing = wait_online(&gw, "mbp", true).await;
    assert!(listing["viewer"].is_null());
    drop(agent);
    gw.shutdown().await;
}

#[tokio::test]
async fn early_viewer_message_is_held_until_join() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let mut viewer = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    viewer.send(Message::Text("early".into())).await.unwrap();
    let session = agent.expect_request().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut data = agent.dial(&session).await.unwrap();
    assert!(matches!(recv(&mut viewer).await, Message::Text(t) if t.contains("\"t\":\"session\"")));
    assert_eq!(recv(&mut data).await, Message::Text("early".into()));
    viewer.send(Message::Text("later".into())).await.unwrap();
    assert_eq!(recv(&mut data).await, Message::Text("later".into()));
    drop((viewer, data, agent));
    gw.shutdown().await;
}

#[tokio::test]
async fn take_over_closes_old_viewer_4001() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut first, mut first_data, first_session) =
        open_session(&gw, &mut agent, "mbp", &[]).await;

    let mut second = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    assert_eq!(expect_close(&mut first).await, 4001);
    // The agent hears about the old session ending before the new request.
    assert_eq!(agent.expect_session_end(&first_session).await, "taken_over");
    let second_session = agent.expect_request().await;
    assert_ne!(second_session, first_session);
    assert_eq!(expect_close(&mut first_data).await, 1000);

    let mut second_data = agent.dial(&second_session).await.unwrap();
    assert!(matches!(recv(&mut second).await, Message::Text(t) if t.contains(&second_session)));
    second_data.send(Message::Text("hi".into())).await.unwrap();
    assert_eq!(recv(&mut second).await, Message::Text("hi".into()));
    // The old session id can no longer be joined.
    assert_eq!(refused_status(agent.dial(&first_session).await), 404);
    drop((second, second_data, agent));
    gw.shutdown().await;
}

#[tokio::test]
async fn unknown_or_offline_machine_4003() {
    let gw = TestGateway::start(config_with(|c| {
        c.machines = vec![MachineConfig {
            machine: "pc".into(),
            name: None,
            recovery: vec![],
        }];
    }))
    .await;
    for machine in ["nope", "pc", "bad.name"] {
        let mut viewer = connect(&gw.viewer_url(&format!("/ws/view/{machine}")), &[])
            .await
            .unwrap();
        assert_eq!(expect_close(&mut viewer).await, 4003, "{machine}");
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_not_dialing_4004() {
    let gw = TestGateway::start(config_with(|c| {
        c.timeouts.session_request = Duration::from_millis(300);
    }))
    .await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let mut viewer = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    let session = agent.expect_request().await;
    assert_eq!(expect_close(&mut viewer).await, 4004);
    assert_eq!(agent.expect_session_end(&session).await, "agent_timeout");
    assert_eq!(refused_status(agent.dial(&session).await), 404);
    drop(agent);
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_going_offline_while_pending_4003() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let mut viewer = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    agent.expect_request().await;
    agent.ctl.close(None).await.unwrap();
    assert_eq!(expect_close(&mut viewer).await, 4003);
    gw.shutdown().await;
}

#[tokio::test]
async fn idle_viewer_4008_only_input_counts() {
    let gw = TestGateway::start(config_with(|c| {
        c.timeouts.idle = Duration::from_millis(400);
    }))
    .await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;

    // Input every 100 ms keeps the session alive well past the idle timeout.
    let mouse = InputEvent::MouseAbs {
        buttons: 0,
        x: 1,
        y: 2,
    }
    .to_message();
    for _ in 0..10 {
        viewer
            .send(Message::Binary(mouse.clone().into()))
            .await
            .unwrap();
        assert_eq!(recv(&mut data).await, Message::Binary(mouse.clone().into()));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // ACKs and control messages are not input.
    let started = tokio::time::Instant::now();
    viewer
        .send(Message::Binary(VideoAck::default().to_message().into()))
        .await
        .unwrap();
    viewer.send(Message::Text("{}".into())).await.unwrap();
    assert_eq!(expect_close(&mut viewer).await, 4008);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(agent.expect_session_end(&session).await, "idle");
    assert_eq!(expect_close(&mut data).await, 1000);
    drop(agent);
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_closing_data_socket_4006() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    data.close(None).await.unwrap();
    assert_eq!(expect_close(&mut viewer).await, 4006);
    assert_eq!(agent.expect_session_end(&session).await, "agent_closed");

    // Application close codes from the agent (4005 = protocol mismatch) reach the viewer as is.
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    data.close(Some(CloseFrame {
        code: 4005.into(),
        reason: "proto_mismatch".into(),
    }))
    .await
    .unwrap();
    assert_eq!(expect_close(&mut viewer).await, 4005);
    assert_eq!(agent.expect_session_end(&session).await, "agent_closed");
    drop(agent);
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_update_ends_session_4010() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();

    // The agent closes its data socket with 4010: passed through, and not an error.
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    data.close(Some(CloseFrame {
        code: close::AGENT_UPDATING.into(),
        reason: "updating".into(),
    }))
    .await
    .unwrap();
    assert_eq!(expect_close(&mut viewer).await, close::AGENT_UPDATING);
    assert_eq!(agent.expect_session_end(&session).await, "agent_update");

    // Its `session_end` on control may arrive first: the viewer still gets 4010.
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    send_json(
        &mut agent.ctl,
        &AgentToGateway::SessionEnd {
            session,
            reason: "agent_update".into(),
        },
    )
    .await;
    assert_eq!(expect_close(&mut viewer).await, close::AGENT_UPDATING);
    assert_eq!(expect_close(&mut data).await, 1000);
    drop(agent);
    gw.shutdown().await;
}

#[tokio::test]
async fn agent_session_end_on_control_4006() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    send_json(
        &mut agent.ctl,
        &AgentToGateway::SessionEnd {
            session: session.clone(),
            reason: "stall".into(),
        },
    )
    .await;
    assert_eq!(expect_close(&mut viewer).await, 4006);
    assert_eq!(expect_close(&mut data).await, 1000);
    // No session_end echoed back: the next control message is the next session's request.
    let viewer2 = connect(&gw.viewer_url("/ws/view/mbp"), &[]).await.unwrap();
    assert!(matches!(
        agent.next_ctl().await,
        GatewayToAgent::SessionRequest { .. }
    ));
    drop((agent, viewer2));
    gw.shutdown().await;
}

fn ping_config() -> Config {
    config_with(|c| {
        c.timeouts.ping_interval = Duration::from_millis(100);
        c.timeouts.pong_timeout = Duration::from_millis(500);
    })
}

#[tokio::test]
async fn unresponsive_viewer_ends_session() {
    let gw = TestGateway::start(ping_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (viewer, mut data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    // The viewer never reads again, so it never answers the gateway's pings. The agent keeps
    // reading both of its sockets (which answers pings), so only the viewer leg times out.
    let (code, reason) = tokio::join!(expect_close(&mut data), agent.expect_session_end(&session));
    assert_eq!(code, 1000);
    assert_eq!(reason, "viewer_unresponsive");
    drop((viewer, agent));
    gw.shutdown().await;
}

#[tokio::test]
async fn unresponsive_agent_leg_ends_session_4006() {
    let gw = TestGateway::start(ping_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, data, session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    // The agent stops reading its data socket but keeps its control socket alive.
    let (code, reason) = tokio::join!(
        expect_close(&mut viewer),
        agent.expect_session_end(&session)
    );
    assert_eq!(code, 4006);
    assert_eq!(reason, "agent_unresponsive");
    drop((data, agent));
    gw.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_sessions_with_1012() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", TOKEN).await.unwrap();
    let (mut viewer, mut data, _session) = open_session(&gw, &mut agent, "mbp", &[]).await;
    let shutdown = tokio::spawn(gw.shutdown());
    let (v, d, c) = tokio::join!(
        expect_close(&mut viewer),
        expect_close(&mut data),
        expect_close(&mut agent.ctl)
    );
    assert_eq!((v, d, c), (1012, 1012, 1012));
    shutdown.await.unwrap();
}
