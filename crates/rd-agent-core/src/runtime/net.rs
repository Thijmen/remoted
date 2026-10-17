//! Outbound WebSocket connections to the gateway.
//!
//! We create the TCP socket ourselves so it gets `TCP_NODELAY` (no Nagle delay on small
//! control/ACK-paced frames) and `TCP_NOTSENT_LOWAT = 32 KiB` (the kernel holds at most that
//! much unsent data, so video waits in the agent, behind the flow window, instead of in a deep
//! socket buffer). TLS is rustls with the ring provider and the webpki roots.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, anyhow, ensure};
use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};
use url::{Host, Url};

pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Unsent bytes the kernel may queue per socket.
const NOTSENT_LOWAT: u32 = 32 * 1024;
/// Per resolved address; the caller bounds the whole dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum DialError {
    /// The upgrade was answered with an HTTP error: from the gateway 401 (bad token), 404
    /// (unknown or expired session), 429 (rate limited), ...; or from the reverse proxy in
    /// front of it 502/503/404 while the gateway is (re)starting.
    #[error("gateway refused the connection: HTTP {status}")]
    Refused {
        status: StatusCode,
        /// `Retry-After` in seconds, if the answer had one.
        retry_after: Option<Duration>,
    },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// Gateway endpoints and credentials.
pub(crate) struct Gateway {
    /// Base URL without a trailing slash, e.g. `wss://rd-agent.example.com`.
    base: String,
    host: Host<String>,
    port: u16,
    auth: HeaderValue,
}

impl Gateway {
    pub(crate) fn new(gateway_url: &str, token: &str) -> anyhow::Result<Self> {
        let url = Url::parse(gateway_url.trim())?;
        ensure!(
            matches!(url.scheme(), "ws" | "wss"),
            "the gateway URL must start with ws:// or wss://"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "the gateway URL must not have a query or fragment"
        );
        let host = url
            .host()
            .ok_or_else(|| anyhow!("the gateway URL has no host"))?
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow!("the gateway URL has no port"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("the agent token contains characters not allowed in an HTTP header")?;
        auth.set_sensitive(true);
        Ok(Gateway {
            base: url.as_str().trim_end_matches('/').to_owned(),
            host,
            port,
            auth,
        })
    }

    pub(crate) fn control_url(&self) -> String {
        format!("{}/ws/agent", self.base)
    }

    /// `session` must already be validated with [`valid_session_id`].
    pub(crate) fn session_url(&self, session: &str) -> String {
        format!("{}/ws/agent/session/{session}", self.base)
    }

    /// Opens a WebSocket to `url` (one of this gateway's endpoints) on a fresh, tuned TCP
    /// connection. Incoming messages are limited to `max_message` bytes.
    pub(crate) async fn dial(&self, url: &str, max_message: usize) -> Result<WsStream, DialError> {
        let mut request = url
            .into_client_request()
            .with_context(|| format!("invalid WebSocket URL {url}"))?;
        let headers = request.headers_mut();
        headers.insert(header::AUTHORIZATION, self.auth.clone());
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(concat!("rd-agent/", env!("CARGO_PKG_VERSION"))),
        );
        let tcp = self.connect_tcp().await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message))
            .max_frame_size(Some(max_message));
        let connector = Connector::Rustls(tls_config());
        match tokio_tungstenite::client_async_tls_with_config(
            request,
            tcp,
            Some(config),
            Some(connector),
        )
        .await
        {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(DialError::Refused {
                status: response.status(),
                retry_after: retry_after(response.headers()),
            }),
            Err(e) => Err(anyhow!(e)
                .context(format!("WebSocket handshake with {url}"))
                .into()),
        }
    }

    async fn connect_tcp(&self) -> anyhow::Result<TcpStream> {
        let addrs: Vec<SocketAddr> = match &self.host {
            Host::Domain(domain) => tokio::net::lookup_host((domain.as_str(), self.port))
                .await
                .with_context(|| format!("resolving {domain}"))?
                .collect(),
            Host::Ipv4(ip) => vec![SocketAddr::from((*ip, self.port))],
            Host::Ipv6(ip) => vec![SocketAddr::from((*ip, self.port))],
        };
        let mut last_err = anyhow!("{} resolved to no addresses", self.host);
        for addr in addrs {
            match tokio::time::timeout(CONNECT_TIMEOUT, connect(addr)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => last_err = anyhow!(e).context(format!("connecting to {addr}")),
                Err(_) => last_err = anyhow!("connecting to {addr}: timed out"),
            }
            debug!("{last_err:#}");
        }
        Err(last_err)
    }
}

/// `Retry-After: <seconds>` (the HTTP-date form is not used by the gateway).
fn retry_after(headers: &tokio_tungstenite::tungstenite::http::HeaderMap) -> Option<Duration> {
    let secs = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    secs.trim().parse().ok().map(Duration::from_secs)
}

/// Session ids come from the gateway (32 hex chars); anything that could escape the URL path
/// is rejected.
pub(crate) fn valid_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    if let Err(e) = tune(&socket) {
        warn!("could not set TCP socket options: {e}");
    }
    socket.connect(addr).await
}

fn tune(socket: &TcpSocket) -> io::Result<()> {
    let sock = socket2::SockRef::from(socket);
    sock.set_tcp_nodelay(true)?;
    set_notsent_lowat(&sock, NOTSENT_LOWAT)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_NOTSENT_LOWAT: libc::c_int = libc::TCP_NOTSENT_LOWAT;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
))]
fn set_notsent_lowat(sock: &socket2::SockRef<'_>, bytes: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let value = libc::c_int::try_from(bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: `value` is a live c_int and the length passed matches its size.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            (&raw const value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Windows has no equivalent option; the flow window alone bounds what is queued.
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
)))]
fn set_notsent_lowat(_sock: &socket2::SockRef<'_>, _bytes: u32) -> io::Result<()> {
    Ok(())
}

/// One client config for the process (shares the TLS session cache across reconnects).
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            // Fails only if another provider is already installed, which is just as good.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let mut cfg = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(cfg)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let g = Gateway::new("wss://rd-agent.example.com/", "t").unwrap();
        assert_eq!(g.control_url(), "wss://rd-agent.example.com/ws/agent");
        assert_eq!(
            g.session_url("ab12"),
            "wss://rd-agent.example.com/ws/agent/session/ab12"
        );
        assert_eq!(g.port, 443);
        let g = Gateway::new("ws://127.0.0.1:8081", "t").unwrap();
        assert_eq!(g.control_url(), "ws://127.0.0.1:8081/ws/agent");
        assert_eq!(g.port, 8081);
        let g = Gateway::new("ws://[::1]:9000/prefix", "t").unwrap();
        assert_eq!(g.control_url(), "ws://[::1]:9000/prefix/ws/agent");
        assert!(Gateway::new("https://x", "t").is_err());
        assert!(Gateway::new("ws://x/?a=b", "t").is_err());
        assert!(Gateway::new("ws://x", "bad\ntoken").is_err());
    }

    #[test]
    fn retry_after_seconds() {
        use tokio_tungstenite::tungstenite::http::HeaderMap;
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(header::RETRY_AFTER, HeaderValue::from_static(" 42"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(42)));
        h.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn session_ids() {
        assert!(valid_session_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../x"));
        assert!(!valid_session_id("a?b"));
    }

    #[tokio::test]
    async fn socket_options_applied() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stream, _) = tokio::join!(connect(addr), listener.accept());
        let stream = stream.unwrap();
        let sock = socket2::SockRef::from(&stream);
        assert!(sock.tcp_nodelay().unwrap());
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd as _;
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: `value`/`len` describe a writable c_int.
            let rc = unsafe {
                libc::getsockopt(
                    sock.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    TCP_NOTSENT_LOWAT,
                    (&raw mut value).cast(),
                    &mut len,
                )
            };
            assert_eq!(rc, 0);
            assert_eq!(value as u32, NOTSENT_LOWAT);
        }
    }
}
