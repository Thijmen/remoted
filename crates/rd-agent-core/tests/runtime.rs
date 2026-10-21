//! End-to-end tests of the agent runtime against an in-process fake gateway + viewer, with a
//! [`common::FakePlatform`] instead of real capture/encode/input.

mod common;

use std::time::{Duration, Instant};

use common::*;
use rd_agent_core::runtime::Timings;
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
        json!({"locked": false, "perms": {"screen": true, "input": true}, "in_session": false, "secure": "unavailable", "disconnected": false})
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
    // Odd and oversized dimensions are clamped: the display's aspect, never above its native
    // 3456x2234, even.
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
        (json!(3456), json!(2234))
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
            if (c.width, c.height, c.fps, c.bitrate_bps) == (3456, 2234, 60, 8_000_000))),
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

    viewer
        .send_burst(["a", "b", "c"].map(|r| json!({"t": "keyframe_req", "reason": r})))
        .await;
    let keys = |frames: &[rd_proto::bin::VideoHeader]| frames.iter().filter(|h| h.is_key()).count();
    let early = viewer.videos_for(Duration::from_millis(350)).await;
    assert_eq!(keys(&early), 1, "keyframes right after 3 requests");
    // The two extra requests arrived while that keyframe was on its way: answered by it.
    let late = viewer.videos_for(Duration::from_millis(800)).await;
    assert_eq!(
        keys(&late),
        0,
        "keyframes for requests a keyframe already answered"
    );

    // A request once its keyframe was delivered is a new one; inside the 500 ms interval it is
    // deferred to the interval's end, and further requests are coalesced with it.
    viewer
        .send(json!({"t": "keyframe_req", "reason": "d"}))
        .await;
    let d = viewer.videos_for(Duration::from_millis(100)).await;
    assert_eq!(
        keys(&d),
        1,
        "a request after the interval is served at once"
    );
    viewer
        .send_burst(["e", "f"].map(|r| json!({"t": "keyframe_req", "reason": r})))
        .await;
    let soon = viewer.videos_for(Duration::from_millis(250)).await;
    assert_eq!(keys(&soon), 0, "rate limited");
    let deferred = viewer.videos_for(Duration::from_millis(800)).await;
    assert_eq!(keys(&deferred), 1, "deferred keyframes");
    assert!(
        early
            .iter()
            .chain(&late)
            .chain(&d)
            .chain(&soon)
            .chain(&deferred)
            .all(|h| h.epoch == 1)
    );

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

    // Older viewers also ask for a keyframe right after resuming.
    viewer
        .send_burst([
            json!({"t": "resume"}),
            json!({"t": "keyframe_req", "reason": "resume"}),
        ])
        .await;
    let resumed = viewer.videos_for(Duration::from_millis(1000)).await;
    assert!(
        resumed.len() >= 20,
        "frames after resume: {}",
        resumed.len()
    );
    assert!(
        resumed[0].is_key(),
        "first frame after resume is a keyframe"
    );
    // One keyframe per resume, not a second one 500 ms later.
    assert_eq!(resumed.iter().filter(|h| h.is_key()).count(), 1);

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
    // Without the permission ScreenCaptureKit refuses to start.
    platform.set_start_failure(Some("the user declined TCCs"));
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
async fn start_streams_when_the_screen_preflight_is_wrong() {
    // The preflight says no, but capture works: the agent streams anyway.
    let platform = FakePlatform::new(100);
    platform.set_perms(Perms {
        screen: false,
        input: true,
    });
    let mut s = setup_with(platform, common::timings()).await;
    let mut viewer = s.open_session(SESSION_A).await;
    viewer.start(1728, 1118).await;
    viewer.expect("video_config").await;
    let first = viewer.next_video().await;
    assert!(first.is_key());
    assert_eq!(
        s.platform.count(|e| matches!(e, Event::PipelineStart(_))),
        1
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_screen_gets_keyframes_through_refresh() {
    let platform = FakePlatform::new(100);
    platform.set_static_screen(true);
    let mut s = setup_with(platform, common::timings()).await;
    let (mut viewer, first) = s.streaming_session(SESSION_A).await;
    assert_eq!(first.flags, KEY_FIRST);
    // Nothing changes on screen: nothing to send.
    let idle = viewer.videos_for(Duration::from_millis(600)).await;
    assert!(idle.is_empty(), "frames of a static screen: {}", idle.len());

    // A decoder that needs a keyframe gets one anyway (the pipeline re-submits its last frame).
    viewer
        .send(json!({"t": "keyframe_req", "reason": "decoder_error"}))
        .await;
    let refreshed = viewer.videos_for(Duration::from_millis(400)).await;
    assert_eq!(refreshed.len(), 1, "{refreshed:?}");
    assert!(refreshed[0].is_key());

    // So does a viewer coming back from a hidden tab: exactly one.
    viewer.videos_for(Duration::from_millis(300)).await;
    viewer.send(json!({"t": "pause"})).await;
    viewer.videos_for(Duration::from_millis(200)).await;
    viewer
        .send_burst([
            json!({"t": "resume"}),
            json!({"t": "keyframe_req", "reason": "resume"}),
        ])
        .await;
    let resumed = viewer.videos_for(Duration::from_millis(1000)).await;
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert!(resumed[0].is_key());

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unparseable_start_and_set_get_bad_request() {
    let mut s = setup().await;
    let mut viewer = s.open_session(SESSION_A).await;
    viewer
        .send(
            json!({"t": "start", "width": 1728, "height": 1118, "fps": 60, "codec": "av1",
                     "start_kbps": 8000, "min_kbps": 1000, "max_kbps": 20000}),
        )
        .await;
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "bad_request");
    let msg = err["msg"].as_str().unwrap();
    assert!(msg.contains("\"start\"") && msg.contains("av1"), "{msg}");
    // A codec the agent knows but does not encode is a bad request too.
    viewer
        .send(
            json!({"t": "start", "width": 1728, "height": 1118, "fps": 60, "codec": "hevc",
                     "start_kbps": 8000, "min_kbps": 1000, "max_kbps": 20000}),
        )
        .await;
    assert_eq!(viewer.expect("error").await["code"], "bad_request");

    viewer.start(1728, 1118).await;
    viewer.expect("video_config").await;
    viewer.send(json!({"t": "set", "width": "wide"})).await;
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "bad_request");
    assert!(err["msg"].as_str().unwrap().contains("\"set\""));
    // Message types from newer viewers are still ignored silently.
    viewer.send(json!({"t": "from_the_future", "x": 1})).await;
    let json = viewer.json_for(Duration::from_millis(300)).await;
    assert!(json.iter().all(|v| v["t"] != "error"), "{json:?}");
    assert!(
        !viewer
            .videos_for(Duration::from_millis(200))
            .await
            .is_empty()
    );

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_failure_is_reported_once_per_failure() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    s.platform
        .set_capture_failure(Some("the display went away"));
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "capture_stopped");
    assert_eq!(err["msg"], "the display went away");
    // Not repeated while it lasts; the session stays open.
    let json = viewer.json_for(Duration::from_millis(2500)).await;
    assert!(json.iter().all(|v| v["t"] != "error"), "{json:?}");

    // Capture recovers, then fails again: reported again.
    s.platform.set_capture_failure(None);
    viewer.json_for(Duration::from_millis(1200)).await;
    s.platform
        .set_capture_failure(Some("screen recording permission revoked"));
    let err = viewer.expect("error").await;
    assert_eq!(err["code"], "capture_stopped");
    assert_eq!(err["msg"], "screen recording permission revoked");

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_is_dropped_while_the_host_is_locked() {
    // No state polling: the report has to come from the dropped input.
    let timings = Timings {
        state_poll: Duration::from_secs(3600),
        ..common::timings()
    };
    let mut s = setup_with(FakePlatform::new(100), timings).await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let key = |down| InputEvent::Key {
        down,
        repeat: false,
        hid: 0x0007_0004,
    };
    let inputs = |p: &FakePlatform| p.count(|e| matches!(e, Event::Input(_)));

    send_bin(&mut viewer.ws, key(true).to_message()).await;
    s.platform
        .wait_for("unlocked input", |e| {
            e.iter().any(|e| matches!(e, Event::Input(_)))
        })
        .await;

    s.platform.set_locked(true);
    // Let the cached (unlocked) state expire.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let releases_before = s.platform.count(|e| *e == Event::ReleaseAll);
    for ev in [
        key(false),
        InputEvent::MouseAbs {
            buttons: 0,
            x: 100,
            y: 100,
        },
        key(true),
        InputEvent::ReleaseAll,
        key(false),
    ] {
        send_bin(&mut viewer.ws, ev.to_message()).await;
    }
    // The viewer is told once that the host is locked; so is the gateway.
    let json = viewer.json_for(Duration::from_millis(600)).await;
    let locked_states: Vec<_> = json
        .iter()
        .filter(|v| v["t"] == "state" && v["locked"] == true)
        .collect();
    assert_eq!(locked_states.len(), 1, "{json:?}");
    s.control.expect_state(|v| v["locked"] == true).await;
    // Nothing reached the host but releases: one when the lock was noticed (the key held down
    // before it), and the viewer's RELEASE_ALL.
    assert_eq!(inputs(&s.platform), 1, "{:?}", s.platform.events());
    assert_eq!(
        s.platform.count(|e| *e == Event::ReleaseAll),
        releases_before + 2
    );
    // More input in the same locked period: dropped, no further report.
    for _ in 0..20 {
        send_bin(&mut viewer.ws, key(true).to_message()).await;
    }
    let json = viewer.json_for(Duration::from_millis(400)).await;
    assert!(json.iter().all(|v| v["t"] != "state"), "{json:?}");
    assert_eq!(inputs(&s.platform), 1);

    // Unlocked: input flows again.
    s.platform.set_locked(false);
    tokio::time::sleep(Duration::from_millis(250)).await;
    send_bin(&mut viewer.ws, key(false).to_message()).await;
    s.platform
        .wait_for("input after unlocking", |e| {
            e.iter().filter(|e| matches!(e, Event::Input(_))).count() == 2
        })
        .await;

    // A new locked period is reported again (the shared state still says locked, so the
    // viewer is told directly).
    s.platform.set_locked(true);
    tokio::time::sleep(Duration::from_millis(250)).await;
    send_bin(&mut viewer.ws, key(true).to_message()).await;
    let json = viewer.json_for(Duration::from_millis(600)).await;
    assert_eq!(
        json.iter()
            .filter(|v| v["t"] == "state" && v["locked"] == true)
            .count(),
        1,
        "{json:?}"
    );
    assert_eq!(inputs(&s.platform), 2);

    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_errors_are_retried_fast_and_refusals_slowly() {
    init_tracing();
    // A long maximum backoff, so the slow path is unmistakable.
    let timings = Timings {
        backoff_max: Duration::from_secs(30),
        ..common::timings()
    };
    let mut gw = FakeGateway::start().await;
    // The gateway pod is still starting: the proxy answers 503, 502, 404.
    gw.reject_controls(&[503, 502, 404]);
    let started = Instant::now();
    let agent = Agent::start(&gw.url, FakePlatform::new(100), timings.clone());
    let control = gw.control().await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "registered after {:?}",
        started.elapsed()
    );

    // A redeploy: the gateway goes away, the proxy answers 503 for a while.
    gw.reject_controls(&[503, 503, 503]);
    let lost = Instant::now();
    drop(control);
    let _control = gw.control().await;
    assert!(
        lost.elapsed() < Duration::from_secs(3),
        "re-registered after {:?}",
        lost.elapsed()
    );
    agent.stop().await;

    // A bad token is refused (401): back off for long.
    let mut gw = FakeGateway::start().await;
    gw.reject_controls(&[401]);
    let agent = Agent::start(&gw.url, FakePlatform::new(100), timings);
    assert!(gw.no_control_within(Duration::from_secs(2)).await);
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_reaches_the_secure_desktop_when_ready() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let key = |down| InputEvent::Key {
        down,
        repeat: false,
        hid: 0x0007_0004,
    };

    s.platform.set_secure(rd_agent_core::SecureState::Ready);
    s.platform.set_locked(true);
    // Let the input thread's cached lock state expire.
    tokio::time::sleep(Duration::from_millis(250)).await;

    send_bin(&mut viewer.ws, key(true).to_message()).await;
    s.platform
        .wait_for("input forwarded to the secure desktop", |e| {
            e.iter().any(|e| matches!(e, Event::Input(_)))
        })
        .await;
    s.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_swaps_to_the_secure_pipeline_while_locked() {
    let mut s = setup().await;
    let (mut viewer, _) = s.streaming_session(SESSION_A).await;
    let starts = |e: &[Event]| {
        e.iter()
            .filter(|e| matches!(e, Event::PipelineStart(_)))
            .count()
    };
    let stops = |e: &[Event]| {
        e.iter()
            .filter(|e| matches!(e, Event::PipelineStop))
            .count()
    };
    assert_eq!(starts(&s.platform.events()), 1);

    s.platform.set_secure(rd_agent_core::SecureState::Starting);
    s.platform.set_locked(true);
    s.platform
        .wait_for("the secure pipeline", |e| starts(e) == 2 && stops(e) == 1)
        .await;
    // The secure pipeline starts a new video epoch (video_config) for the viewer.
    viewer.expect("video_config").await;

    s.platform.set_locked(false);
    s.platform
        .wait_for("the normal pipeline again", |e| {
            starts(e) == 3 && stops(e) == 2
        })
        .await;
    viewer.expect("video_config").await;
    s.agent.stop().await;
}
