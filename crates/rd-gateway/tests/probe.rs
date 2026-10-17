//! Network probe: `/ws/probe` tests and the HTTP fallbacks.

mod common;

use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use rd_proto::bin::{self, BinMsg, Ping, VideoAck};
use rd_proto::control::{ProbeCtl, ProbeEvt, ProbeTest};
use tokio_tungstenite::tungstenite::Message;

async fn probe_socket(gw: &TestGateway) -> Ws {
    let mut ws = connect(&gw.viewer_url("/ws/probe"), &[]).await.unwrap();
    match recv_json::<ProbeEvt>(&mut ws).await {
        ProbeEvt::Hello {
            server_time_us,
            client_ip,
            user,
        } => {
            assert!(server_time_us > 0);
            assert_eq!(client_ip, "127.0.0.1");
            assert_eq!(user, "dev@localhost");
        }
        other => panic!("expected hello, got {other:?}"),
    }
    ws
}

fn run(test: ProbeTest, kbps: u32, fps: u32, seconds: u32, windowed: bool) -> ProbeCtl {
    ProbeCtl::Run {
        test,
        kbps,
        fps,
        seconds,
        windowed,
    }
}

fn ack(seq: u32) -> Message {
    Message::Binary(
        VideoAck {
            seq,
            ..Default::default()
        }
        .to_message()
        .into(),
    )
}

/// Drive a down test: ACK frames (if `ack_frames`), answer gateway PINGs, and check frame
/// layout. Returns (frames received, bytes received, done event).
async fn drive_down(
    ws: &mut Ws,
    payload_len: Option<usize>,
    ack_frames: bool,
) -> (u64, u64, ProbeEvt) {
    let (mut frames, mut bytes, mut last_seq) = (0u64, 0u64, 0u32);
    loop {
        match recv(ws).await {
            Message::Binary(b) => match bin::parse(&b).unwrap() {
                BinMsg::Video(h, payload) => {
                    assert!(h.seq > last_seq, "seq increases");
                    assert_eq!((h.flags, h.epoch, h.encode_us), (0, 0, 0));
                    if let Some(len) = payload_len {
                        assert_eq!(payload.len(), len);
                    }
                    last_seq = h.seq;
                    frames += 1;
                    bytes += b.len() as u64;
                    if ack_frames {
                        ws.send(ack(h.seq)).await.unwrap();
                    }
                }
                BinMsg::Ping(p) => {
                    let pong = p.pong(1).to_message();
                    ws.send(Message::Binary(pong.into())).await.unwrap();
                }
                other => panic!("unexpected {other:?}"),
            },
            Message::Text(t) => {
                let evt: ProbeEvt = serde_json::from_str(&t).unwrap();
                return (frames, bytes, evt);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn probe_ws_ping_down_up_idle_stop() {
    let gw = TestGateway::start(dev_config()).await;
    let mut ws = probe_socket(&gw).await;

    let ping = Ping {
        id: 7,
        t_origin_us: 42,
    };
    ws.send(Message::Binary(ping.to_message().into()))
        .await
        .unwrap();
    match recv(&mut ws).await {
        Message::Binary(b) => match bin::parse(&b).unwrap() {
            BinMsg::Pong(p) => {
                assert_eq!((p.id, p.t_origin_us), (7, 42));
                assert!(p.t_reply_us > 0);
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }

    // down: 400 kbps at 20 fps = 2500-byte payloads for 1 s.
    send_json(&mut ws, &run(ProbeTest::Down, 400, 20, 1, false)).await;
    let (frames, bytes, done) = drive_down(&mut ws, Some(2500), true).await;
    match done {
        ProbeEvt::Done {
            test: ProbeTest::Down,
            elapsed_ms,
            frames: f,
            bytes: b,
            window_drops: 0,
            stopped: false,
        } => {
            assert_eq!((f, b), (frames, bytes));
            assert!((15..=22).contains(&frames), "frames {frames}");
            assert_eq!(bytes, frames * 2520);
            assert!((1000..1500).contains(&elapsed_ms), "elapsed {elapsed_ms}");
        }
        other => panic!("{other:?}"),
    }

    // up: everything sent before the deadline is counted.
    send_json(&mut ws, &run(ProbeTest::Up, 1000, 60, 1, false)).await;
    for _ in 0..50 {
        let m = bin::probe_up_message(&[0xAB; 1000]);
        ws.send(Message::Binary(m.into())).await.unwrap();
    }
    match recv_json::<ProbeEvt>(&mut ws).await {
        ProbeEvt::Done {
            test: ProbeTest::Up,
            frames,
            bytes,
            stopped: false,
            ..
        } => assert_eq!((frames, bytes), (50, 50 * 1002)),
        other => panic!("{other:?}"),
    }

    // idle: done after `seconds`, nothing sent in between.
    let t0 = tokio::time::Instant::now();
    send_json(&mut ws, &run(ProbeTest::Idle, 0, 60, 1, false)).await;
    match recv_json::<ProbeEvt>(&mut ws).await {
        ProbeEvt::Done {
            test: ProbeTest::Idle,
            frames: 0,
            stopped: false,
            ..
        } => assert!(t0.elapsed() >= Duration::from_secs(1)),
        other => panic!("{other:?}"),
    }

    // one run at a time; stop ends it early.
    send_json(&mut ws, &run(ProbeTest::Idle, 0, 60, 600, false)).await;
    send_json(&mut ws, &run(ProbeTest::Down, 100, 10, 5, false)).await;
    assert!(matches!(recv_json(&mut ws).await, ProbeEvt::Error { code, .. } if code == "busy"));
    send_json(&mut ws, &ProbeCtl::Stop).await;
    assert!(matches!(
        recv_json(&mut ws).await,
        ProbeEvt::Done {
            test: ProbeTest::Idle,
            stopped: true,
            ..
        }
    ));

    // limits and garbage.
    for bad in [
        serde_json::to_string(&run(ProbeTest::Down, 100_001, 60, 10, false)).unwrap(),
        serde_json::to_string(&run(ProbeTest::Idle, 0, 60, 7201, false)).unwrap(),
        r#"{"t":"run","test":"sideways","seconds":1}"#.to_owned(),
        "not json".to_owned(),
    ] {
        ws.send(Message::Text(bad.clone().into())).await.unwrap();
        assert!(
            matches!(recv_json(&mut ws).await, ProbeEvt::Error { code, .. } if code == "bad_request"),
            "{bad}"
        );
    }
    drop(ws);
    gw.shutdown().await;
}

#[tokio::test]
async fn probe_ws_windowed_down_respects_ack_window() {
    let gw = TestGateway::start(dev_config()).await;
    let mut ws = probe_socket(&gw).await;

    // ACKing every frame: the window never fills, frames flow at ~fps.
    send_json(&mut ws, &run(ProbeTest::Down, 2000, 30, 1, true)).await;
    let (frames, _, done) = drive_down(&mut ws, None, true).await;
    assert!(frames >= 20, "acked windowed run delivered {frames} frames");
    assert!(
        matches!(
            done,
            ProbeEvt::Done {
                window_drops: 0,
                ..
            }
        ),
        "{done:?}"
    );

    // Never ACKing: at most one window (≤ 16 frames) is sent, the rest are window drops.
    send_json(&mut ws, &run(ProbeTest::Down, 2000, 30, 1, true)).await;
    let (frames, _, done) = drive_down(&mut ws, None, false).await;
    assert!(
        (1..=16).contains(&frames),
        "unacked windowed run sent {frames} frames"
    );
    match done {
        ProbeEvt::Done { window_drops, .. } => assert!(window_drops >= 10, "{window_drops}"),
        other => panic!("{other:?}"),
    }
    drop(ws);
    gw.shutdown().await;
}

#[tokio::test]
async fn probe_http_down_up_report() {
    let gw = TestGateway::start(dev_config()).await;

    // 800 kbps in 50 ms chunks = 5000-byte chunks for 1 s.
    let r = get(
        gw.viewer,
        "/api/probe/down?kbps=800&seconds=1&chunk_ms=50",
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(r.header("cache-control"), Some("no-store, no-transform"));
    assert_eq!(r.body.len() % 5000, 0);
    let chunks: Vec<&[u8]> = r.body.chunks(5000).collect();
    assert!((15..=22).contains(&chunks.len()), "{} chunks", chunks.len());
    let mut last_us = 0;
    for (i, c) in chunks.iter().enumerate() {
        let us = u64::from_le_bytes(c[..8].try_into().unwrap());
        let seq = u32::from_le_bytes(c[8..12].try_into().unwrap());
        assert_eq!(seq as usize, i + 1);
        assert!(us >= last_us);
        last_us = us;
    }
    for bad in [
        "/api/probe/down?kbps=0",
        "/api/probe/down?kbps=100001",
        "/api/probe/down?seconds=7201",
        "/api/probe/down?chunk_ms=0",
        "/api/probe/down?kbps=abc",
    ] {
        assert_eq!(get(gw.viewer, bad, &[]).await.status, 400, "{bad}");
    }

    let r = http(gw.viewer, "POST", "/api/probe/up", &[], vec![7u8; 3 << 20]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["bytes"], 3 << 20);
    assert!(r.json()["elapsed_ms"].is_u64());

    let ok = br#"{"downs":[1,2,3],"note":"line\nbreak"}"#.to_vec();
    assert_eq!(
        http(gw.viewer, "POST", "/api/probe/report", &[], ok)
            .await
            .status,
        204
    );
    let r = http(
        gw.viewer,
        "POST",
        "/api/probe/report",
        &[],
        b"{nope".to_vec(),
    )
    .await;
    assert_eq!(r.status, 400);
    // One byte over the 1 MiB limit (the server reads it all, so the client never races a
    // connection close while still writing).
    let mut huge = b"[".to_vec();
    huge.extend(std::iter::repeat_n(b'1', (1 << 20) - 1));
    huge.push(b']');
    let r = http(gw.viewer, "POST", "/api/probe/report", &[], huge).await;
    assert_eq!(r.status, 413);
    gw.shutdown().await;
}
