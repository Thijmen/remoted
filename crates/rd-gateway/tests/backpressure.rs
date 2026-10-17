//! A viewer that stops reading must push back on the agent instead of making the gateway
//! buffer: the agent's sends stall after a bounded number of in-flight bytes, and once the
//! viewer reads again every message arrives, in order and unmodified.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

const MSG_LEN: usize = 256 << 10;
/// 100 MiB attempted — far more than any sane amount of socket buffering.
const TOTAL: u64 = 400;
/// Everything between the agent's `send` and the viewer's `recv`: four kernel socket buffers,
/// tungstenite's read buffer and at most one message in the gateway's hands per direction.
const MAX_IN_FLIGHT: u64 = 40 << 20;

fn message(seq: u64) -> Vec<u8> {
    let mut m = vec![(seq % 251) as u8; MSG_LEN];
    m[..8].copy_from_slice(&seq.to_le_bytes());
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_viewer_backpressures_agent() {
    let gw = TestGateway::start(dev_config()).await;
    let mut agent = FakeAgent::connect(&gw, "mbp", "rd1.mbp.dev").await.unwrap();
    let (mut viewer, mut data, _session) = open_session(&gw, &mut agent, "mbp", &[]).await;

    let sent = Arc::new(AtomicU64::new(0));
    let sender = {
        let sent = Arc::clone(&sent);
        tokio::spawn(async move {
            for seq in 0..TOTAL {
                data.send(Message::Binary(message(seq).into()))
                    .await
                    .expect("agent send");
                sent.store(seq + 1, Ordering::SeqCst);
            }
            data
        })
    };

    // The viewer is not reading. Wait until the agent makes no progress for a full second.
    let mut last = u64::MAX;
    let stalled_at = loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let now = sent.load(Ordering::SeqCst);
        assert!(
            now < TOTAL,
            "agent sent everything to a viewer that never read"
        );
        if now == last {
            break now;
        }
        last = now;
    };
    let in_flight = stalled_at * MSG_LEN as u64;
    eprintln!(
        "agent stalled after {stalled_at} messages = {:.1} MiB in flight",
        in_flight as f64 / f64::from(1 << 20)
    );
    assert!(
        in_flight <= MAX_IN_FLIGHT,
        "{in_flight} bytes in flight exceeds {MAX_IN_FLIGHT}"
    );

    // Resume: every message arrives exactly once, in order, unmodified.
    for seq in 0..TOTAL {
        match recv(&mut viewer).await {
            Message::Binary(b) => {
                assert_eq!(b.len(), MSG_LEN, "message {seq} length");
                assert_eq!(&b[..], &message(seq)[..], "message {seq} content");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let data = tokio::time::timeout(WAIT, sender)
        .await
        .expect("sender finished")
        .unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), TOTAL);
    drop((viewer, data, agent));
    gw.shutdown().await;
}
