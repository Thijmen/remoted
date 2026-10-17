//! Small shared helpers: clocks, client IP extraction, WebSocket close, task tracking.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::HeaderMap;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Notify;

/// How long closing a WebSocket may take (sending our Close and waiting for the reply).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic gateway clock in microseconds since start (probe timestamps, PONG replies).
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            start: Instant::now(),
        }
    }

    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// Wall clock, milliseconds since the Unix epoch (portal `since_ms`).
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Client IP: the rightmost `X-Forwarded-For` entry (the one our own proxy appended; anything
/// left of it is client-controlled), else the TCP peer.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| parse_ip(v.trim()))
        .unwrap_or_else(|| peer.ip())
        .to_canonical()
}

fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
}

/// Gracefully close a WebSocket: send our Close frame, then drain until the peer's Close (or
/// EOF) so the frame is not lost to a TCP reset. Bounded by [`CLOSE_TIMEOUT`].
pub async fn close_ws(socket: WebSocket, code: u16, reason: &'static str) {
    let (tx, rx) = socket.split();
    close_split(tx, rx, code, reason).await;
}

/// [`close_ws`] for a socket that has been split. The connection is dropped on return.
pub async fn close_split(
    mut tx: SplitSink<WebSocket, Message>,
    mut rx: SplitStream<WebSocket>,
    code: u16,
    reason: &'static str,
) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        // Fails harmlessly when the peer closed first (the reply is then already queued and is
        // flushed by the reads below).
        let _ = tx.send(Message::Close(Some(frame))).await;
        while let Some(Ok(msg)) = rx.next().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// Counts live WebSocket tasks so shutdown can wait for them to close their sockets.
#[derive(Debug, Default)]
pub struct TaskTracker {
    live: AtomicUsize,
    idle: Notify,
}

#[must_use = "the task is only tracked while the guard is alive"]
pub struct TaskGuard(Arc<TaskTracker>);

impl TaskTracker {
    pub fn track(self: &Arc<Self>) -> TaskGuard {
        self.live.fetch_add(1, Ordering::SeqCst);
        TaskGuard(Arc::clone(self))
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Resolves once no tracked task is alive.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.live() == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.0.live.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn rightmost_forwarded_for_wins() {
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h, peer), "10.0.0.1".parse::<IpAddr>().unwrap());
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 1.2.3.4"),
        );
        assert_eq!(client_ip(&h, peer), "1.2.3.4".parse::<IpAddr>().unwrap());
        h.append("x-forwarded-for", HeaderValue::from_static("5.6.7.8:99"));
        assert_eq!(client_ip(&h, peer), "5.6.7.8".parse::<IpAddr>().unwrap());
        h.append("x-forwarded-for", HeaderValue::from_static("garbage"));
        assert_eq!(client_ip(&h, peer), "10.0.0.1".parse::<IpAddr>().unwrap());
        let mapped: SocketAddr = "[::ffff:9.9.9.9]:1".parse().unwrap();
        assert_eq!(
            client_ip(&HeaderMap::new(), mapped),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn tracker_waits_for_guards() {
        let t = Arc::new(TaskTracker::default());
        t.wait_idle().await;
        let g = t.track();
        let t2 = Arc::clone(&t);
        let waiter = tokio::spawn(async move { t2.wait_idle().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(g);
        waiter.await.unwrap();
        assert_eq!(t.live(), 0);
    }
}
