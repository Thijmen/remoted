//! The runtime's update hookup (docs/updates.md) against the in-process fake gateway: the
//! version the binary sets, `update_status` reports, `update` / `registered` events, and
//! sessions ended (close 4010) while the binary holds them for an update.

mod common;

use std::time::Duration;

use common::*;
use rd_agent_core::runtime::{UpdateEvent, Updates, update_link};
use rd_proto::control::{UpdatePhase, UpdateStatus};
use serde_json::json;

const SESSION_C: &str = "cccccccccccccccccccccccccccccccc";

async fn setup_updates() -> (Setup, Updates) {
    init_tracing();
    let (link, updates) = update_link(UpdateStatus {
        can_install: true,
        ..Default::default()
    });
    let mut gw = FakeGateway::start().await;
    let platform = FakePlatform::new(100);
    let agent = Agent::start_with(&gw.url, platform.clone(), timings(), |runtime| {
        runtime.version("9.8.7").updates(link)
    });
    let control = gw.control().await;
    let setup = Setup {
        gw,
        control,
        agent,
        platform,
    };
    (setup, updates)
}

async fn next_event(updates: &mut Updates) -> UpdateEvent {
    tokio::time::timeout(WAIT, updates.events.recv())
        .await
        .expect("no update event")
        .expect("the runtime dropped its end")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_the_binary_version_and_update_status() {
    let (mut s, mut updates) = setup_updates().await;
    assert_eq!(s.control.register["agent_version"], "9.8.7");

    // Once registered: the status, and an event for the binary (which writes its marker).
    let status = s.control.expect("update_status").await;
    assert_eq!(status["can_install"], true);
    assert_eq!(status["phase"], "idle");
    match next_event(&mut updates).await {
        UpdateEvent::Registered { state } => assert!(state.perms.screen && !state.in_session),
        other => panic!("{other:?}"),
    }

    // Every change is reported.
    updates
        .status
        .send_modify(|s| s.phase = UpdatePhase::Downloading);
    let status = s.control.expect("update_status").await;
    assert_eq!(status["phase"], "downloading");

    // `update` reaches the binary.
    send_json(
        &mut s.control.ws,
        json!({"t": "update", "version": "9.9.0"}),
    )
    .await;
    assert_eq!(
        next_event(&mut updates).await,
        UpdateEvent::Install {
            version: "9.9.0".into()
        }
    );

    // Viewers see the binary's version too.
    s.control.request_session(SESSION_A).await;
    let mut viewer = Viewer::new(s.gw.session().await.ws);
    let hello = viewer.hello().await;
    assert_eq!(hello["agent"]["version"], "9.8.7");

    // After a reconnect the status is sent again, after `registered`.
    s.control.ws.close(None).await.unwrap();
    let mut control = s.gw.control().await;
    let status = control.expect("update_status").await;
    assert_eq!(status["phase"], "downloading");
    assert!(matches!(
        next_event(&mut updates).await,
        UpdateEvent::Registered { .. }
    ));
    s.control = control;

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_ends_sessions_with_4010_until_released() {
    let (mut s, updates) = setup_updates().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;

    // The binary reports that it is downloading, then ends the session.
    updates
        .status
        .send_modify(|s| s.phase = UpdatePhase::Downloading);
    let sessions = updates.sessions.clone();
    let hold = tokio::spawn(async move { sessions.hold(WAIT).await });
    assert_eq!(viewer.expect_close().await, Some(4010));
    // The gateway has the phase before the session ends, so a viewer that asks after the 4010
    // does not see an idle agent and reconnect to it.
    let mut downloading = false;
    let end = loop {
        match recv(&mut s.control.ws).await {
            Msg::Json(v) if v["t"] == "update_status" => {
                downloading |= v["phase"] == "downloading";
            }
            Msg::Json(v) if v["t"] == "session_end" => break v,
            Msg::Json(_) => {}
            other => panic!("{other:?}"),
        }
    };
    assert!(downloading, "update_status came after session_end");
    assert_eq!(end["session"], SESSION_A);
    assert_eq!(end["reason"], "agent_update");
    s.control.expect_state(|v| v["in_session"] == false).await;
    let hold = tokio::time::timeout(WAIT, hold).await.unwrap().unwrap();
    // The gateway may end the already ended session as well: nothing happens.
    send_json(
        &mut s.control.ws,
        json!({"t": "session_end", "session": SESSION_A, "reason": "agent_update"}),
    )
    .await;

    // New sessions are turned away the same way while the hold lives.
    s.control.request_session(SESSION_B).await;
    let mut viewer = Viewer::new(s.gw.session().await.ws);
    assert_eq!(viewer.expect_close().await, Some(4010));
    let end = s.control.expect("session_end").await;
    assert_eq!(end["session"], SESSION_B);
    assert_eq!(end["reason"], "agent_update");
    send_json(
        &mut s.control.ws,
        json!({"t": "session_end", "session": SESSION_B, "reason": "agent_update"}),
    )
    .await;

    // Released (the update failed before the switch): sessions work again.
    drop(hold);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut viewer = s.open_session(SESSION_C).await;
    viewer.start(1728, 1118).await;
    viewer.expect("video_config").await;

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_hookup_update_is_ignored() {
    let mut s = setup().await;
    send_json(
        &mut s.control.ws,
        json!({"t": "update", "version": "9.9.0"}),
    )
    .await;
    // Still serving, and nothing about updates was sent.
    s.platform.set_locked(true);
    loop {
        match recv(&mut s.control.ws).await {
            Msg::Json(v) if v["t"] == "state" => break,
            Msg::Json(v) => assert_ne!(v["t"], "update_status", "{v}"),
            other => panic!("{other:?}"),
        }
    }
    s.agent.stop().await;
}
