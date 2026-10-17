//! `RD_CHAOS` failure injection (dev only): models the agent→viewer path as a FIFO bottleneck
//! link with a bandwidth cap plus a fixed one-way delay.
//!
//! [`ChaosStream`] wraps the agent's message stream. It reads ahead into a small delay line that
//! is bounded in bytes (so backpressure still reaches the agent once it is full) and releases
//! each message, in order, at `max(arrival + delay, link free) + size / bandwidth`. Nothing is
//! dropped or reordered.

use std::collections::VecDeque;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail};
use axum::extract::ws::Message;
use futures_util::{Stream, StreamExt};
use tokio::time::{Instant, Sleep};

/// Parsed `RD_CHAOS` spec: `bw_kbps=5000,delay_ms=50` (either key optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Chaos {
    pub bw_kbps: Option<u32>,
    pub delay_ms: u32,
}

impl FromStr for Chaos {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chaos = Chaos::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| anyhow!("expected key=value, got {part:?}"))?;
            let value: u32 = value
                .trim()
                .parse()
                .map_err(|_| anyhow!("{key}: not a number: {value:?}"))?;
            match key.trim() {
                "bw_kbps" if value > 0 => chaos.bw_kbps = Some(value),
                "bw_kbps" => bail!("bw_kbps must be > 0"),
                "delay_ms" => chaos.delay_ms = value,
                other => bail!("unknown RD_CHAOS key {other:?} (known: bw_kbps, delay_ms)"),
            }
        }
        Ok(chaos)
    }
}

/// Upper bound on bytes held in the delay line (at least this, or what the configured
/// bandwidth-delay product needs).
const MIN_QUEUE_BYTES: usize = 4 << 20;
/// Accounting size for zero-length control frames so the queue length stays bounded too.
const MIN_ITEM_BYTES: usize = 64;

struct Line<T> {
    delay: Duration,
    /// Bytes per second; `None` = unlimited.
    rate: Option<f64>,
    queue: VecDeque<(Instant, usize, T)>,
    queued_bytes: usize,
    max_queued_bytes: usize,
    /// When the simulated link finishes transmitting the previous message.
    link_free_at: Instant,
    /// Release time of the queue head, once computed (computing it reserves link time).
    head_release: Option<Instant>,
    sleep: Pin<Box<Sleep>>,
    inner_done: bool,
}

/// Stream adaptor applying [`Chaos`] to an inner message stream; a pass-through when disabled.
pub struct ChaosStream<S, E> {
    inner: S,
    line: Option<Line<Result<Message, E>>>,
}

impl<S, E> ChaosStream<S, E> {
    pub fn new(inner: S, chaos: Option<Chaos>) -> Self {
        let line = chaos.map(|c| {
            let rate = c.bw_kbps.map(|kbps| f64::from(kbps) * 1000.0 / 8.0);
            let delay = Duration::from_millis(u64::from(c.delay_ms));
            let bdp = rate.map_or(0.0, |r| r * delay.as_secs_f64() * 2.0) as usize;
            Line {
                delay,
                rate,
                queue: VecDeque::new(),
                queued_bytes: 0,
                max_queued_bytes: bdp.max(MIN_QUEUE_BYTES),
                link_free_at: Instant::now(),
                head_release: None,
                sleep: Box::pin(tokio::time::sleep(Duration::ZERO)),
                inner_done: false,
            }
        });
        ChaosStream { inner, line }
    }
}

fn payload_len<E>(item: &Result<Message, E>) -> usize {
    match item {
        Ok(Message::Binary(b)) => b.len(),
        Ok(Message::Text(t)) => t.len(),
        _ => 0,
    }
}

impl<S, E> Stream for ChaosStream<S, E>
where
    S: Stream<Item = Result<Message, E>> + Unpin,
    E: Unpin,
{
    type Item = Result<Message, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(line) = this.line.as_mut() else {
            return this.inner.poll_next_unpin(cx);
        };

        // Read ahead into the delay line while it has room.
        while !line.inner_done && line.queued_bytes < line.max_queued_bytes {
            match this.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(item)) => {
                    let len = payload_len(&item);
                    line.queued_bytes += len.max(MIN_ITEM_BYTES);
                    line.queue
                        .push_back((Instant::now() + line.delay, len, item));
                }
                Poll::Ready(None) => line.inner_done = true,
                Poll::Pending => break,
            }
        }

        loop {
            let Some(&(due, len, _)) = line.queue.front() else {
                return if line.inner_done {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                };
            };
            let release = *line.head_release.get_or_insert_with(|| match line.rate {
                Some(rate) if len > 0 => {
                    let start = due.max(line.link_free_at);
                    line.link_free_at = start + Duration::from_secs_f64(len as f64 / rate);
                    line.link_free_at
                }
                _ => due,
            });
            if Instant::now() >= release {
                line.head_release = None;
                let (_, len, item) = line.queue.pop_front().expect("queue head checked above");
                line.queued_bytes -= len.max(MIN_ITEM_BYTES);
                return Poll::Ready(Some(item));
            }
            line.sleep.as_mut().reset(release);
            if line.sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[test]
    fn parse() {
        assert_eq!(
            "bw_kbps=5000,delay_ms=50".parse::<Chaos>().unwrap(),
            Chaos {
                bw_kbps: Some(5000),
                delay_ms: 50
            }
        );
        assert_eq!(
            " delay_ms = 7 ".parse::<Chaos>().unwrap(),
            Chaos {
                bw_kbps: None,
                delay_ms: 7
            }
        );
        assert!("bw_kbps=0".parse::<Chaos>().is_err());
        assert!("loss=5".parse::<Chaos>().is_err());
        assert!("delay_ms".parse::<Chaos>().is_err());
        assert!("delay_ms=x".parse::<Chaos>().is_err());
    }

    fn msgs(sizes: &[usize]) -> Vec<Result<Message, ()>> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let mut v = vec![0u8; n];
                if let Some(b) = v.first_mut() {
                    *b = i as u8;
                }
                Ok(Message::Binary(v.into()))
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn passthrough_when_disabled() {
        let mut s = ChaosStream::new(stream::iter(msgs(&[1, 2, 3])), None);
        let t0 = Instant::now();
        let mut n = 0;
        while s.next().await.is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
        assert_eq!(Instant::now(), t0);
    }

    #[tokio::test(start_paused = true)]
    async fn delay_and_bandwidth_keep_order() {
        // 8 kbps = 1000 bytes/s: two 500-byte messages take 0.5 s each on the link.
        let chaos = Chaos {
            bw_kbps: Some(8),
            delay_ms: 100,
        };
        let mut s = ChaosStream::new(stream::iter(msgs(&[500, 500, 0])), Some(chaos));
        let t0 = Instant::now();
        let mut out = Vec::new();
        while let Some(Ok(Message::Binary(b))) = s.next().await {
            out.push((b.len(), (Instant::now() - t0).as_millis()));
        }
        // first: 100 ms delay + 500 ms transmission; second queues behind it; the zero-length
        // message only pays the delay but still comes last.
        assert_eq!(out, vec![(500, 600), (500, 1100), (0, 1100)]);
    }
}
