//! The runtime's activity (connection state, who is connected) and pausing remote access on
//! the machine, against the in-process fake gateway.

mod common;

use std::time::Duration;

use common::*;
use rd_agent_core::runtime::{Activity, Connection, PresenceKind};
use tokio::sync::watch;

const SESSION_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn wait_activity(
    rx: &mut watch::Receiver<Activity>,
    what: &str,
    pred: impl Fn(&Activity) -> bool,
) -> Activity {
    let found = match tokio::time::timeout(WAIT, rx.wait_for(|a| pred(a))).await {
        Ok(Ok(a)) => Some(a.clone()),
        Ok(Err(_)) => panic!("activity sender dropped while waiting for {what}"),
        Err(_) => None,
    };
    found.unwrap_or_else(|| panic!("timed out waiting for {what}; last: {:?}", *rx.borrow()))
}

/// Reads the control socket until it closes; returns the `t` of every JSON message before.
async fn until_closed(ws: &mut Ws) -> (Vec<String>, Option<u16>) {
    let mut seen = Vec::new();
    let read = async {
        loop {
            match recv(ws).await {
                Msg::Json(v) => seen.push(v["t"].as_str().unwrap_or_default().to_owned()),
                Msg::Bin(_) => {}
                Msg::Close(code) => return code,
            }
        }
    };
    let code = tokio::time::timeout(WAIT, read)
        .await
        .expect("the control socket did not close");
    (seen, code)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_disconnects_until_resumed() {
    init_tracing();
    let (activity_tx, mut activity) = watch::channel(Activity::default());
    let (paused_tx, paused) = watch::channel(false);
    let mut gw = FakeGateway::start().await;
    let platform = FakePlatform::new(100);
    let agent = Agent::start_with(&gw.url, platform.clone(), timings(), |runtime| {
        runtime.activity(activity_tx).pause(paused)
    });
    let control = gw.control().await;
    wait_activity(&mut activity, "online", |a| {
        a.connection == Connection::Online
    })
    .await;
    let mut s = Setup {
        gw,
        control,
        agent,
        platform,
    };

    // A connected viewer is listed with the gateway's name for them.
    let mut viewer = s.open_session(SESSION_A).await;
    let a = wait_activity(&mut activity, "the viewer", |a| !a.present.is_empty()).await;
    assert_eq!(a.present.len(), 1);
    assert_eq!(a.present[0].kind, PresenceKind::Screen);
    assert_eq!(a.present[0].user, "me@test");

    // Pausing ends the session, reports it, and hangs up.
    paused_tx.send_replace(true);
    viewer.expect_close().await;
    let (seen, code) = until_closed(&mut s.control.ws).await;
    assert!(seen.iter().any(|t| t == "session_end"), "{seen:?}");
    assert_eq!(code, Some(1001));
    let a = wait_activity(&mut activity, "paused", |a| {
        a.connection == Connection::Paused
    })
    .await;
    assert!(a.present.is_empty(), "{a:?}");
    assert!(
        s.gw.no_control_within(Duration::from_millis(600)).await,
        "a paused agent must not reconnect"
    );

    // Resuming reconnects.
    paused_tx.send_replace(false);
    let _control = s.gw.control().await;
    wait_activity(&mut activity, "online again", |a| {
        a.connection == Connection::Online
    })
    .await;

    s.agent.stop().await;
    assert_eq!(activity.borrow().connection, Connection::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_from_the_start_and_offline_reasons() {
    init_tracing();
    let (activity_tx, mut activity) = watch::channel(Activity::default());
    let (paused_tx, paused) = watch::channel(true);
    let mut gw = FakeGateway::start().await;
    gw.reject_controls(&[503, 503]);
    let agent = Agent::start_with(&gw.url, FakePlatform::new(100), timings(), |runtime| {
        runtime.activity(activity_tx).pause(paused)
    });
    wait_activity(&mut activity, "paused", |a| {
        a.connection == Connection::Paused
    })
    .await;
    assert!(gw.no_control_within(Duration::from_millis(300)).await);

    // While the gateway is not there, the reason is kept between attempts.
    paused_tx.send_replace(false);
    let a = wait_activity(&mut activity, "offline", |a| {
        matches!(a.connection, Connection::Offline { .. })
    })
    .await;
    assert_eq!(
        a.connection,
        Connection::Offline {
            reason: "the gateway is unavailable".into()
        }
    );
    let _control = gw.control().await;
    wait_activity(&mut activity, "online", |a| {
        a.connection == Connection::Online
    })
    .await;

    // Pausing while connected, then dropping the pause hookup, resumes for good.
    paused_tx.send_replace(true);
    wait_activity(&mut activity, "paused", |a| {
        a.connection == Connection::Paused
    })
    .await;
    drop(paused_tx);
    let _control = gw.control().await;
    wait_activity(&mut activity, "online", |a| {
        a.connection == Connection::Online
    })
    .await;
    agent.stop().await;
}
