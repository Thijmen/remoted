//! End-to-end tests of the agent runtime against an in-process fake gateway + viewer, with a
//! [`common::FakePlatform`] instead of real capture/encode/input.

mod common;

use std::time::Duration;

use common::*;
use rd_agent_core::{InputEvent, Perms};
use rd_proto::bin::{self, BinMsg, MouseButton, Ping, ScrollPhase, WheelMode, video_flags};
use serde_json::json;

const KEY_FIRST: u8 = video_flags::KEY | video_flags::FIRST_OF_EPOCH;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registers_and_reports_state_changes() {
    let mut s = setup().await;
    let reg = &s.control.register;
    assert_eq!(reg["machine"], "mbp");
    assert_eq!(reg["gw_proto"], 1);
    assert_eq!(reg["os"], "FakeOS 1.0");
    assert_eq!(reg["agent_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        reg["state"],
        json!({"locked": false, "perms": {"screen": true, "input": true}, "in_session": false})
    );

    s.platform.set_locked(true);
    let state = s.control.expect("state").await;
    assert_eq!(state["locked"], true);

    s.platform.set_perms(Perms {
        screen: false,
        input: true,
    });
    let state = s.control.expect("state").await;
    assert_eq!(state["perms"]["screen"], false);
    assert_eq!(state["locked"], true);

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_request_dials_and_negotiates_hello() {
    let mut s = setup().await;
    s.control.request_session(SESSION_A).await;
    let conn = s.gw.session().await;
    assert_eq!(conn.path, format!("/ws/agent/session/{SESSION_A}"));
    assert_eq!(conn.auth.as_deref(), Some("Bearer rd1.mbp.secret"));
    s.control.expect_state(|v| v["in_session"] == true).await;

    let mut viewer = Viewer::new(conn.ws);
    // A newer viewer: the agent answers with the highest common version.
    viewer
        .send(json!({"t": "hello", "proto": 7, "min_proto": 1, "viewer": {"ua": "test"}}))
        .await;
    let hello = viewer.expect("hello").await;
    assert_eq!(hello["proto"], 1);
    assert_eq!(hello["agent"]["os"], "fakeos");
    assert_eq!(hello["agent"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        hello["agent"]["displays"],
        json!([{"id": 0, "w_pt": 1728, "h_pt": 1117, "scale": 2.0}])
    );
    let state = viewer.expect("state").await;
    assert_eq!(state["in_session"], true);

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_mismatch_is_rejected_with_4005() {
    let mut s = setup().await;
    s.control.request_session(SESSION_A).await;
    let mut viewer = Viewer::new(s.gw.session().await.ws);
    viewer
        .send(json!({"t": "hello", "proto": 9, "min_proto": 5}))
        .await;
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "proto_mismatch");
    assert_eq!(viewer.expect_close().await, Some(4005));
    let end = s.control.expect("session_end").await;
    assert_eq!(end["session"], SESSION_A);
    assert_eq!(end["reason"], "proto_mismatch");
    s.control.expect_state(|v| v["in_session"] == false).await;

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_config_precedes_the_first_keyframe() {
    let mut s = setup().await;
    let mut viewer = s.open_session(SESSION_A).await;
    // Odd and oversized dimensions are clamped: aspect kept, within the native 3456x2234, even.
    viewer.start(7001, 4469).await;
    let config = loop {
        match viewer.next().await {
            Msg::Json(v) if v["t"] == "video_config" => break v,
            Msg::Json(_) => {}
            other => panic!("expected video_config before any video, got {other:?}"),
        }
    };
    assert_eq!(config["epoch"], 1);
    assert_eq!(config["codec"], "avc1.640033");
    assert_eq!(
        (config["width"].clone(), config["height"].clone()),
        (json!(3456), json!(2206))
    );
    assert_eq!(config["description"], "AWQAM//hAAA=");
    let first = viewer.next_video().await;
    assert_eq!(first.flags, KEY_FIRST);
    assert_eq!(first.epoch, 1);
    assert_eq!(first.seq, 1);
    let second = viewer.next_video().await;
    assert_eq!((second.flags, second.epoch, second.seq), (0, 1, 2));

    let events = s.platform.events();
    assert!(
        events.iter().any(|e| matches!(e, Event::PipelineStart(c)
            if (c.width, c.height, c.fps, c.bitrate_bps) == (3456, 2206, 60, 8_000_000))),
        "{events:?}"
    );
    assert!(events.contains(&Event::AwakeAcquired));
    assert!(events.contains(&Event::UserActivity));

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_stops_frames_without_acks_and_resumes_after_acks() {
    let mut s = setup().await;
    let mut viewer = s.open_session(SESSION_A).await;
    viewer.auto_ack = false;
    viewer.start(1728, 1118).await;
    viewer.expect("video_config").await;

    // 100 fps capture, no ACKs: the window (≤ 16 frames) fills and frames stop.
    let held = viewer.videos_for(Duration::from_millis(600)).await;
    assert!(
        (2..=16).contains(&held.len()),
        "frames while not acking: {}",
        held.len()
    );
    let more = viewer.videos_for(Duration::from_millis(400)).await;
    assert!(more.is_empty(), "frames kept coming: {}", more.len());

    // A cumulative ACK of the newest frame opens the window again.
    viewer.ack(held.last().unwrap()).await;
    viewer.auto_ack = true;
    let resumed = viewer.videos_for(Duration::from_millis(500)).await;
    assert!(
        resumed.len() >= 10,
        "frames after acking: {}",
        resumed.len()
    );
    assert!(resumed[0].seq > held.last().unwrap().seq);
    // Sequence numbers are consecutive (no frame is lost after encoding).
    for pair in resumed.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1);
    }

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pings_are_answered_and_the_agent_pings() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let ping = Ping {
        id: 77,
        t_origin_us: 123_456,
    };
    send_bin(&mut viewer.ws, ping.to_message()).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while viewer.pongs.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no PONG");
        viewer.next().await;
    }
    let pong = viewer.pongs[0];
    assert_eq!((pong.id, pong.t_origin_us), (77, 123_456));
    assert!(pong.t_reply_us > 0);

    // The agent pings by itself (every 500 ms) while streaming.
    viewer.answer_pings = false;
    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1600);
    while tokio::time::Instant::now() < deadline {
        if let Msg::Bin(b) = common::recv(&mut viewer.ws).await
            && matches!(bin::parse(&b), Ok(BinMsg::Ping(_)))
        {
            pings += 1;
        }
    }
    assert!((2..=5).contains(&pings), "agent pings in 1.6 s: {pings}");

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyframe_requests_are_rate_limited() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    // Let the stream settle past the epoch's own keyframe.
    viewer.videos_for(Duration::from_millis(600)).await;

    for reason in ["a", "b", "c"] {
        viewer
            .send(json!({"t": "keyframe_req", "reason": reason}))
            .await;
    }
    let early = viewer.videos_for(Duration::from_millis(350)).await;
    let early_keys = early.iter().filter(|h| h.is_key()).count();
    assert_eq!(early_keys, 1, "keyframes right after 3 requests");
    // The two extra requests are coalesced into one, due 500 ms after the first.
    let late = viewer.videos_for(Duration::from_millis(1000)).await;
    let late_keys = late.iter().filter(|h| h.is_key()).count();
    assert_eq!(late_keys, 1, "deferred keyframes");
    assert!(early.iter().chain(&late).all(|h| h.epoch == 1));

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_starts_a_new_epoch() {
    let mut s = setup().await;
    let (mut viewer, first) = s.streaming_session(SESSION_A).await;
    assert_eq!(first.epoch, 1);
    viewer
        .send(json!({"t": "set", "width": 1280, "height": 828}))
        .await;
    let config = viewer.expect("video_config").await;
    assert_eq!(config["epoch"], 2);
    assert_eq!(
        (config["width"].clone(), config["height"].clone()),
        (json!(1280), json!(828))
    );
    let next = viewer.next_video().await;
    assert_eq!((next.epoch, next.flags), (2, KEY_FIRST));
    let after = viewer.next_video().await;
    assert_eq!((after.epoch, after.flags), (2, 0));
    assert!(
        s.platform.events().iter().any(
            |e| matches!(e, Event::PipelineReconfigure(c) if (c.width, c.height) == (1280, 828))
        )
    );

    // A bitrate-only change does not start an epoch.
    viewer.send(json!({"t": "set", "max_kbps": 3000})).await;
    let frames = viewer.videos_for(Duration::from_millis(400)).await;
    assert!(
        frames
            .iter()
            .all(|h| h.epoch == 2 && h.flags & video_flags::FIRST_OF_EPOCH == 0)
    );
    assert_eq!(
        s.platform
            .count(|e| matches!(e, Event::PipelineReconfigure(_))),
        1
    );

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_keeps_its_order_and_is_released_when_the_session_ends() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let mut sent = Vec::new();
    for i in 0..200u16 {
        let ev = match i % 5 {
            0 => InputEvent::MouseAbs {
                buttons: 0,
                x: i * 300,
                y: i,
            },
            1 => InputEvent::Key {
                down: true,
                repeat: false,
                hid: 0x0007_0004 + u32::from(i),
            },
            2 => InputEvent::Key {
                down: false,
                repeat: false,
                hid: 0x0007_0004 + u32::from(i - 1),
            },
            3 => InputEvent::MouseButton {
                button: MouseButton::Left,
                down: i % 2 == 0,
                x: i,
                y: i,
            },
            _ => InputEvent::Wheel {
                mode: WheelMode::Pixel,
                dx: 0,
                dy: i as i16,
                phase: ScrollPhase::Changed,
            },
        };
        sent.push(ev);
        send_bin(&mut viewer.ws, ev.to_message()).await;
    }
    send_bin(&mut viewer.ws, InputEvent::ReleaseAll.to_message()).await;
    let expected = sent.len() + 1;
    s.platform
        .wait_for("all input", |events| {
            events
                .iter()
                .filter(|e| matches!(e, Event::Input(_) | Event::ReleaseAll))
                .count()
                >= expected
        })
        .await;
    let injected: Vec<InputEvent> = s
        .platform
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Input(ev) => Some(ev),
            _ => None,
        })
        .collect();
    assert_eq!(injected, sent);
    // The viewer's RELEASE_ALL maps to release_all().
    assert_eq!(s.platform.count(|e| *e == Event::ReleaseAll), 1);

    // Viewer leaves: input released, capture stopped, keep-awake dropped, gateway told.
    viewer.ws.close(None).await.unwrap();
    let end = s.control.expect("session_end").await;
    assert_eq!(end["reason"], "viewer_closed");
    s.platform
        .wait_for("teardown", |events| {
            events.contains(&Event::PipelineStop) && events.contains(&Event::AwakeReleased)
        })
        .await;
    let events = s.platform.events();
    let last_input = events
        .iter()
        .rposition(|e| matches!(e, Event::Input(_)))
        .unwrap();
    let last_release = events
        .iter()
        .rposition(|e| *e == Event::ReleaseAll)
        .unwrap();
    assert!(
        last_release > last_input,
        "release_all after the last input"
    );
    assert!(s.platform.count(|e| *e == Event::ReleaseAll) >= 2);
    s.control.expect_state(|v| v["in_session"] == false).await;

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_session_request_replaces_the_old_session() {
    let mut s = setup().await;
    let (mut old, _) = s.streaming_session(SESSION_A).await;

    s.control.request_session(SESSION_B).await;
    assert_eq!(old.expect_close().await, Some(1000));
    let conn = s.gw.session().await;
    assert_eq!(conn.path, format!("/ws/agent/session/{SESSION_B}"));
    let mut viewer = Viewer::new(conn.ws);
    viewer.hello().await;
    viewer.start(1728, 1118).await;
    let config = viewer.expect("video_config").await;
    assert_eq!(config["epoch"], 1);
    let first = viewer.next_video().await;
    assert_eq!((first.flags, first.seq), (KEY_FIRST, 1));

    // The old pipeline stopped before the new one started; nothing overlapped.
    let events = s.platform.events();
    let starts: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::PipelineStart(_)))
        .map(|(i, _)| i)
        .collect();
    let stop = events
        .iter()
        .position(|e| *e == Event::PipelineStop)
        .unwrap();
    assert_eq!(starts.len(), 2);
    assert!(starts[0] < stop && stop < starts[1], "{events:?}");

    // The gateway is not told about a replacement it asked for.
    viewer.ws.close(None).await.unwrap();
    let end = s.control.expect("session_end").await;
    assert_eq!(end["session"], SESSION_B);

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_session_end_stops_the_session() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    send_json(
        &mut s.control.ws,
        json!({"t": "session_end", "session": SESSION_A, "reason": "idle"}),
    )
    .await;
    assert_eq!(viewer.expect_close().await, Some(1000));
    s.platform
        .wait_for("pipeline stop", |e| e.contains(&Event::PipelineStop))
        .await;
    // Only a state update follows, no session_end echo.
    loop {
        match common::recv(&mut s.control.ws).await {
            Msg::Json(v) if v["t"] == "state" && v["in_session"] == false => break,
            Msg::Json(v) => assert_ne!(v["t"], "session_end"),
            other => panic!("{other:?}"),
        }
    }

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_stops_video_and_resume_restarts_it_with_a_keyframe() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    viewer.send(json!({"t": "pause"})).await;
    // Frames already admitted may still arrive; then nothing.
    viewer.videos_for(Duration::from_millis(300)).await;
    let paused = viewer.videos_for(Duration::from_millis(500)).await;
    assert!(paused.is_empty(), "frames while paused: {}", paused.len());

    viewer.send(json!({"t": "resume"})).await;
    let resumed = viewer.videos_for(Duration::from_millis(500)).await;
    assert!(
        resumed.len() >= 10,
        "frames after resume: {}",
        resumed.len()
    );
    assert!(
        resumed[0].is_key(),
        "first frame after resume is a keyframe"
    );

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_viewer_session_is_ended_with_4006() {
    let timings = rd_agent_core::runtime::Timings {
        stall_warn: Duration::from_millis(300),
        stall_end: Duration::from_millis(1000),
        ..common::timings()
    };
    let mut s = setup_with(FakePlatform::new(100), timings).await;
    let mut viewer = s.open_session(SESSION_A).await;
    viewer.auto_ack = false;
    viewer.start(1728, 1118).await;
    assert_eq!(viewer.expect_close().await, Some(4006));
    let end = s.control.expect("session_end").await;
    assert_eq!(end["reason"], "stall");

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_without_screen_permission_reports_tcc_screen() {
    let platform = FakePlatform::new(100);
    platform.set_perms(Perms {
        screen: false,
        input: false,
    });
    let mut s = setup_with(platform, common::timings()).await;
    let mut viewer = s.open_session(SESSION_A).await;
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "tcc_input");
    viewer.start(1728, 1118).await;
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "tcc_screen");
    assert_eq!(
        s.platform.count(|e| matches!(e, Event::PipelineStart(_))),
        0
    );

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_the_control_socket_drops() {
    let mut s = setup().await;
    // A live session survives the control reconnect.
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    s.control.ws.close(None).await.unwrap();
    drop(s.control);
    let mut control = s.gw.control().await;
    assert_eq!(control.register["machine"], "mbp");
    assert_eq!(control.register["state"]["in_session"], true);
    let frames = viewer.videos_for(Duration::from_millis(300)).await;
    assert!(!frames.is_empty(), "session kept streaming");

    // And again, after an abrupt drop without a close frame.
    drop(control);
    control = s.gw.control().await;
    assert_eq!(control.register["t"], "register");
    s.control = control;

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_the_session_cleanly() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let stop = tokio::spawn(s.agent.stop());
    assert_eq!(viewer.expect_close().await, Some(1001));
    // Reported while still connected, then the control socket is closed politely.
    let end = s.control.expect("session_end").await;
    assert_eq!(end["reason"], "agent_shutdown");
    loop {
        if let Msg::Close(code) = common::recv(&mut s.control.ws).await {
            assert_eq!(code, Some(1001));
            break;
        }
    }
    stop.await.unwrap();
    let events = s.platform.events();
    assert!(events.contains(&Event::PipelineStop));
    assert!(events.contains(&Event::ReleaseAll));
    assert!(events.contains(&Event::AwakeReleased));
    assert!(s.gw.no_control_within(Duration::from_millis(500)).await);
}
