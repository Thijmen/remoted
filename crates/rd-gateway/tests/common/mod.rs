//! Shared helpers for the gateway integration tests: an in-process gateway on ephemeral
//! loopback ports, WebSocket/HTTP clients and a scripted fake agent.

#![allow(dead_code)] // each test binary uses a different subset

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use rd_gateway::{Config, Gateway, Timeouts};
use rd_proto::control::{AgentState, AgentToGateway, GatewayToAgent, GatewayToViewer};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Default wait for anything a test expects to happen.
pub const WAIT: Duration = Duration::from_secs(5);

/// Fast timers for tests; individual tests override what they exercise.
pub fn test_timeouts() -> Timeouts {
    Timeouts {
        ping_interval: Duration::from_secs(30),
        pong_timeout: Duration::from_secs(60),
        session_request: Duration::from_secs(5),
        idle: Duration::from_secs(600),
        register: Duration::from_secs(5),
    }
}

pub fn dev_config() -> Config {
    Config {
        timeouts: test_timeouts(),
        ..Config::dev_ephemeral()
    }
}

/// A temporary agent-token directory (removed on drop).
pub struct TokenDir {
    pub path: PathBuf,
}

impl TokenDir {
    pub fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rd-gw-test-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        TokenDir { path }
    }

    pub fn add(&self, machine: &str, token: &str) {
        std::fs::write(
            self.path.join(machine),
            format!("{}\n", hex::encode(Sha256::digest(token.as_bytes()))),
        )
        .unwrap();
    }
}

impl Drop for TokenDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub struct TestGateway {
    pub viewer: SocketAddr,
    pub agent: SocketAddr,
    pub metrics: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TestGateway {
    pub async fn start(cfg: Config) -> Self {
        let gw = Gateway::bind(cfg).await.expect("bind gateway");
        let (viewer, agent, metrics) = (gw.viewer_addr(), gw.agent_addr(), gw.metrics_addr());
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(gw.run(async {
            let _ = stopped.await;
        }));
        TestGateway {
            viewer,
            agent,
            metrics,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub fn viewer_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.viewer)
    }

    pub fn agent_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.agent)
    }

    /// Graceful shutdown; returns once the gateway has finished.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("gateway shut down in time")
                .expect("gateway task")
                .expect("gateway result");
        }
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket client helpers

pub async fn connect(url: &str, headers: &[(&str, &str)]) -> Result<Ws, tungstenite::Error> {
    connect_with(url, headers, None).await
}

pub async fn connect_with(
    url: &str,
    headers: &[(&str, &str)],
    config: Option<WebSocketConfig>,
) -> Result<Ws, tungstenite::Error> {
    let mut req = url.into_client_request()?;
    for (k, v) in headers {
        req.headers_mut().append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async_with_config(req, config, true).await?;
    Ok(ws)
}

/// HTTP status of a refused WebSocket handshake.
pub fn refused_status(res: Result<Ws, tungstenite::Error>) -> u16 {
    match res {
        Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => panic!("expected an HTTP refusal, got a WebSocket"),
    }
}

/// Next data (text/binary) or close message; WebSocket pings/pongs are skipped.
pub async fn recv(ws: &mut Ws) -> Message {
    tokio::time::timeout(WAIT, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(msg)) => return msg,
                Some(Err(err)) => panic!("websocket error: {err}"),
                None => panic!("websocket ended"),
            }
        }
    })
    .await
    .expect("timed out waiting for a message")
}

pub async fn recv_json<T: DeserializeOwned>(ws: &mut Ws) -> T {
    match recv(ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap_or_else(|e| panic!("{e}: {t}")),
        other => panic!("expected text, got {other:?}"),
    }
}

pub async fn send_json<T: serde::Serialize>(ws: &mut Ws, msg: &T) {
    ws.send(Message::Text(serde_json::to_string(msg).unwrap().into()))
        .await
        .unwrap();
}

/// Read until the peer's Close frame and return its code (data messages are discarded), then
/// finish the close handshake.
pub async fn expect_close(ws: &mut Ws) -> u16 {
    tokio::time::timeout(WAIT, async {
        let code = loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => {
                    break frame.map_or(1005, |f| u16::from(f.code));
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("websocket error before close: {err}"),
                None => panic!("websocket ended without a close frame"),
            }
        };
        // Reading on flushes our close reply and waits for the server to drop the connection.
        while let Some(Ok(_)) = ws.next().await {}
        code
    })
    .await
    .expect("timed out waiting for close")
}

// ---------------------------------------------------------------------------
// fake agent

pub struct FakeAgent {
    pub ctl: Ws,
    agent_addr: SocketAddr,
    token: String,
}

impl FakeAgent {
    pub async fn connect(
        gw: &TestGateway,
        machine: &str,
        token: &str,
    ) -> Result<FakeAgent, tungstenite::Error> {
        Self::connect_with_caps(gw, machine, token, &[]).await
    }

    /// Registers with `register.caps` (e.g. `["files"]`).
    pub async fn connect_with_caps(
        gw: &TestGateway,
        machine: &str,
        token: &str,
        caps: &[&str],
    ) -> Result<FakeAgent, tungstenite::Error> {
        let mut agent = Self::connect_unregistered(gw, token).await?;
        agent.register_with_caps(machine, caps).await;
        let registered: GatewayToAgent = recv_json(&mut agent.ctl).await;
        assert_eq!(registered, GatewayToAgent::Registered { gw_proto: 1 });
        Ok(agent)
    }

    pub async fn connect_unregistered(
        gw: &TestGateway,
        token: &str,
    ) -> Result<FakeAgent, tungstenite::Error> {
        let auth = format!("Bearer {token}");
        let ctl = connect(&gw.agent_url("/ws/agent"), &[("authorization", &auth)]).await?;
        Ok(FakeAgent {
            ctl,
            agent_addr: gw.agent,
            token: token.to_owned(),
        })
    }

    pub async fn register(&mut self, machine: &str) {
        self.register_with_caps(machine, &[]).await;
    }

    pub async fn register_with_caps(&mut self, machine: &str, caps: &[&str]) {
        let register = AgentToGateway::Register {
            machine: machine.into(),
            agent_version: "0.1.0".into(),
            os: "macOS 26.3 (test)".into(),
            gw_proto: 1,
            state: AgentState::default(),
            caps: caps.iter().map(|c| (*c).to_owned()).collect(),
        };
        send_json(&mut self.ctl, &register).await;
    }

    pub async fn next_ctl(&mut self) -> GatewayToAgent {
        recv_json(&mut self.ctl).await
    }

    /// Wait for a `session_request` and return its session id.
    pub async fn expect_request(&mut self) -> String {
        match self.next_ctl().await {
            GatewayToAgent::SessionRequest { session, .. } => session,
            other => panic!("expected session_request, got {other:?}"),
        }
    }

    pub async fn expect_session_end(&mut self, session: &str) -> String {
        match self.next_ctl().await {
            GatewayToAgent::SessionEnd { session: s, reason } if s == session => reason,
            other => panic!("expected session_end for {session}, got {other:?}"),
        }
    }

    /// Wait for a `files_request` and return its channel id.
    pub async fn expect_files_request(&mut self) -> String {
        match self.next_ctl().await {
            GatewayToAgent::FilesRequest { channel, .. } => channel,
            other => panic!("expected files_request, got {other:?}"),
        }
    }

    /// Wait for a `terminal_request` and return its channel id.
    pub async fn expect_terminal_request(&mut self) -> String {
        match self.next_ctl().await {
            GatewayToAgent::TerminalRequest { channel, .. } => channel,
            other => panic!("expected terminal_request, got {other:?}"),
        }
    }

    pub async fn dial_terminal(&self, channel: &str) -> Result<Ws, tungstenite::Error> {
        let auth = format!("Bearer {}", self.token);
        let url = format!("ws://{}/ws/agent/terminal/{channel}", self.agent_addr);
        connect_with(&url, &[("authorization", &auth)], None).await
    }

    pub async fn dial_files(&self, channel: &str) -> Result<Ws, tungstenite::Error> {
        let auth = format!("Bearer {}", self.token);
        let url = format!("ws://{}/ws/agent/files/{channel}", self.agent_addr);
        connect_with(&url, &[("authorization", &auth)], None).await
    }

    pub async fn dial(&self, session: &str) -> Result<Ws, tungstenite::Error> {
        self.dial_with(session, None).await
    }

    pub async fn dial_with(
        &self,
        session: &str,
        config: Option<WebSocketConfig>,
    ) -> Result<Ws, tungstenite::Error> {
        let auth = format!("Bearer {}", self.token);
        let url = format!("ws://{}/ws/agent/session/{session}", self.agent_addr);
        connect_with(&url, &[("authorization", &auth)], config).await
    }
}

/// Connect a viewer and complete the join; returns (viewer, agent data socket, session id).
pub async fn open_session(
    gw: &TestGateway,
    agent: &mut FakeAgent,
    machine: &str,
    viewer_headers: &[(&str, &str)],
) -> (Ws, Ws, String) {
    let mut viewer = connect(
        &gw.viewer_url(&format!("/ws/view/{machine}")),
        viewer_headers,
    )
    .await
    .expect("viewer connects");
    let session = agent.expect_request().await;
    let data = agent.dial(&session).await.expect("agent dials session");
    match recv_json::<GatewayToViewer>(&mut viewer).await {
        GatewayToViewer::Session {
            machine: m,
            session: s,
            ..
        } => {
            assert_eq!(m, machine);
            assert_eq!(s, session);
        }
        other => panic!("expected session, got {other:?}"),
    }
    (viewer, data, session)
}

/// Open a file channel and complete the join; returns (viewer, agent file socket, channel id).
pub async fn open_files(
    gw: &TestGateway,
    agent: &mut FakeAgent,
    machine: &str,
    viewer_headers: &[(&str, &str)],
) -> (Ws, Ws, String) {
    let mut viewer = connect(
        &gw.viewer_url(&format!("/ws/files/{machine}")),
        viewer_headers,
    )
    .await
    .expect("viewer connects");
    let channel = agent.expect_files_request().await;
    let data = agent.dial_files(&channel).await.expect("agent dials files");
    match recv_json::<GatewayToViewer>(&mut viewer).await {
        GatewayToViewer::Session {
            machine: m,
            session: s,
            ..
        } => {
            assert_eq!(m, machine);
            assert_eq!(s, channel);
        }
        other => panic!("expected session, got {other:?}"),
    }
    (viewer, data, channel)
}

/// A viewer opens `/ws/terminal/{machine}`, the agent dials its terminal channel, the viewer
/// gets `session`. Returns (viewer, agent data socket, channel id).
pub async fn open_terminal(
    gw: &TestGateway,
    agent: &mut FakeAgent,
    machine: &str,
    viewer_headers: &[(&str, &str)],
) -> (Ws, Ws, String) {
    let mut viewer = connect(
        &gw.viewer_url(&format!("/ws/terminal/{machine}")),
        viewer_headers,
    )
    .await
    .expect("viewer connects");
    let channel = agent.expect_terminal_request().await;
    let data = agent
        .dial_terminal(&channel)
        .await
        .expect("agent dials terminal");
    match recv_json::<GatewayToViewer>(&mut viewer).await {
        GatewayToViewer::Session {
            machine: m,
            session: s,
            ..
        } => {
            assert_eq!(m, machine);
            assert_eq!(s, channel);
        }
        other => panic!("expected session, got {other:?}"),
    }
    (viewer, data, channel)
}

// ---------------------------------------------------------------------------
// HTTP client

pub struct HttpResponse {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("json body")
    }
}

pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> HttpResponse {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = sender
        .send_request(req.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = tokio::time::timeout(Duration::from_secs(30), resp.into_body().collect())
        .await
        .expect("body in time")
        .unwrap()
        .to_bytes()
        .to_vec();
    HttpResponse {
        status,
        headers,
        body,
    }
}

pub async fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    http(addr, "GET", path, headers, Vec::new()).await
}
