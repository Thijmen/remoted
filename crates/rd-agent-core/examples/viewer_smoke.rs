//! End-to-end smoke test through a running gateway: plays a minimal viewer on
//! `{viewer}/ws/view/{machine}`, negotiates hello/start, receives `video_config` and VIDEO
//! frames, ACKs each frame on receipt, answers agent PINGs, and reports frame rate, bitrate
//! and capture→receive latency (via the PING/PONG clock offset). Sends no input.
//!
//! ```text
//! rd-gateway --dev &
//! RD_AGENT_TOKEN=rd1.mbp.dev rd-agent run --gateway ws://127.0.0.1:8081 --machine mbp --source synthetic &
//! cargo run -p rd-agent-core --example viewer_smoke -- --frames 300
//! ```

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use clap::Parser;
use futures_util::{SinkExt as _, StreamExt as _};
use rd_proto::bin::{self, BinMsg, Ping, VideoAck, video_flags};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    /// Gateway viewer base URL.
    #[arg(long, default_value = "ws://127.0.0.1:8080")]
    viewer: String,
    #[arg(long, default_value = "mbp")]
    machine: String,
    /// Stop after this many VIDEO frames.
    #[arg(long, default_value_t = 300)]
    frames: u32,
    #[arg(long, default_value_t = 1728)]
    width: u32,
    #[arg(long, default_value_t = 1118)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 8000)]
    start_kbps: u32,
    #[arg(long, default_value_t = 20000)]
    max_kbps: u32,
}

/// Viewer clock (µs since start), like `performance.now()`.
struct Clock(Instant);

impl Clock {
    fn now_us(&self) -> u64 {
        self.0.elapsed().as_micros() as u64
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let clock = Clock(Instant::now());
    let url = format!(
        "{}/ws/view/{}",
        args.viewer.trim_end_matches('/'),
        args.machine
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    println!("connected to {url}");

    let session = next_json(&mut ws).await?;
    ensure!(session["t"] == "session", "expected session, got {session}");
    println!("gateway session: {session}");
    send_json(
        &mut ws,
        json!({"t": "hello", "proto": 1, "min_proto": 1,
               "viewer": {"ua": "viewer_smoke", "dpr": 2, "viewport": [1536, 750], "decoders": {"avc": true}}}),
    )
    .await?;
    let hello = next_json(&mut ws).await?;
    ensure!(hello["t"] == "hello", "expected hello, got {hello}");
    println!("agent hello: {hello}");
    send_json(
        &mut ws,
        json!({"t": "start", "width": args.width, "height": args.height, "fps": args.fps, "codec": "avc",
               "start_kbps": args.start_kbps, "min_kbps": 1000, "max_kbps": args.max_kbps, "adaptive": true}),
    )
    .await?;

    let mut epoch: Option<u16> = None;
    let mut expect_first = false;
    let mut frames = 0u32;
    let mut keyframes = 0u32;
    let mut bytes = 0u64;
    let mut last_seq: Option<u32> = None;
    let mut first_at: Option<Instant> = None;
    let mut last_at = Instant::now();
    // (rtt, offset = agent - viewer) of the best PONG so far.
    let mut best_pong: Option<(u64, i64)> = None;
    let mut latencies_us: Vec<i64> = Vec::new();
    let mut ping = tokio::time::interval(Duration::from_millis(500));
    let mut ping_id = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    while frames < args.frames {
        let msg = tokio::select! {
            msg = ws.next() => msg,
            _ = ping.tick() => {
                ping_id += 1;
                let p = Ping { id: ping_id, t_origin_us: clock.now_us() };
                ws.send(Message::Binary(p.to_message().into())).await?;
                continue;
            }
            () = tokio::time::sleep_until(deadline) => bail!("timed out after {frames} frames"),
        };
        let msg = match msg {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => bail!("socket error after {frames} frames: {e}"),
            None => bail!("socket closed after {frames} frames"),
        };
        match msg {
            Message::Text(text) => {
                let v: Value = serde_json::from_str(text.as_str())?;
                match v["t"].as_str() {
                    Some("video_config") => {
                        let avcc = base64::engine::general_purpose::STANDARD
                            .decode(v["description"].as_str().unwrap_or_default())?;
                        println!(
                            "video_config epoch {} {} {}x{} (avcC {} bytes)",
                            v["epoch"],
                            v["codec"],
                            v["width"],
                            v["height"],
                            avcc.len()
                        );
                        epoch = v["epoch"].as_u64().map(|e| e as u16);
                        expect_first = true;
                    }
                    Some("stats") => println!("stats: {v}"),
                    Some("error") => println!("agent error: {v}"),
                    _ => println!("message: {v}"),
                }
            }
            Message::Binary(data) => match bin::parse(&data) {
                Ok(BinMsg::Video(h, payload)) => {
                    let now = Instant::now();
                    let recv_us = clock.now_us();
                    ensure!(
                        Some(h.epoch) == epoch,
                        "frame of epoch {} without its video_config",
                        h.epoch
                    );
                    if expect_first {
                        ensure!(
                            h.flags & (video_flags::FIRST_OF_EPOCH | video_flags::KEY)
                                == video_flags::FIRST_OF_EPOCH | video_flags::KEY,
                            "first frame of epoch {} has flags {:#x}",
                            h.epoch,
                            h.flags
                        );
                        expect_first = false;
                    }
                    if let Some(prev) = last_seq {
                        ensure!(
                            h.seq == prev.wrapping_add(1),
                            "seq jumped {prev} -> {}",
                            h.seq
                        );
                    }
                    last_seq = Some(h.seq);
                    let ack = VideoAck {
                        flags: 0,
                        epoch: h.epoch,
                        seq: h.seq,
                        decode_us: 0,
                        queue_us: 0,
                    };
                    ws.send(Message::Binary(ack.to_message().into())).await?;
                    if let Some((_, offset)) = best_pong {
                        latencies_us.push(recv_us as i64 + offset - h.capture_us as i64);
                    }
                    frames += 1;
                    keyframes += u32::from(h.is_key());
                    bytes += payload.len() as u64;
                    first_at.get_or_insert(now);
                    last_at = now;
                }
                Ok(BinMsg::Ping(p)) => {
                    let pong = p.pong(clock.now_us());
                    ws.send(Message::Binary(pong.to_message().into())).await?;
                }
                Ok(BinMsg::Pong(p)) => {
                    let t_recv = clock.now_us();
                    let rtt = t_recv.saturating_sub(p.t_origin_us);
                    let offset = p.t_reply_us as i64 - ((p.t_origin_us + t_recv) / 2) as i64;
                    if best_pong.is_none_or(|(best, _)| rtt < best) {
                        best_pong = Some((rtt, offset));
                    }
                }
                Ok(_) => {}
                Err(e) => println!("malformed binary message: {e}"),
            },
            Message::Close(frame) => {
                bail!("closed by the gateway after {frames} frames: {frame:?}")
            }
            _ => {}
        }
    }
    let _ = ws.close(None).await;

    let secs = first_at.map_or(0.0, |t| last_at.duration_since(t).as_secs_f64());
    let fps = if secs > 0.0 {
        f64::from(frames - 1) / secs
    } else {
        0.0
    };
    latencies_us.sort_unstable();
    let pct = |p: usize| {
        latencies_us
            .get((latencies_us.len() * p / 100).min(latencies_us.len().saturating_sub(1)))
            .map_or(f64::NAN, |&us| us as f64 / 1000.0)
    };
    println!(
        "--- {frames} frames in {secs:.2} s: {fps:.1} fps, {:.0} kbps, {keyframes} keyframes",
        bytes as f64 * 8.0 / 1000.0 / secs.max(1e-9)
    );
    println!(
        "capture->receive latency: p50 {:.1} ms, p95 {:.1} ms (rtt {:.2} ms, {} samples)",
        pct(50),
        pct(95),
        best_pong.map_or(f64::NAN, |(rtt, _)| rtt as f64 / 1000.0),
        latencies_us.len()
    );
    Ok(())
}

async fn send_json<S>(ws: &mut S, v: Value) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    ws.send(Message::Text(v.to_string().into())).await?;
    Ok(())
}

async fn next_json<S>(ws: &mut S) -> anyhow::Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .context("timed out waiting for a message")?;
        match msg {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(t.as_str())?),
            Some(Ok(Message::Close(f))) => bail!("closed: {f:?}"),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => bail!("closed"),
        }
    }
}
