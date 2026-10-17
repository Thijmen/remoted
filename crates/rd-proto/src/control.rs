//! JSON control messages (WebSocket text frames), tagged by `"t"`.
//!
//! Channels:
//! - viewer <-> agent: [`ViewerMsg`] / [`AgentMsg`], passed through the gateway unchanged.
//! - gateway -> viewer: [`GatewayToViewer`] (`session`, `gw_error`), on the same viewer socket.
//! - agent <-> gateway control socket: [`AgentToGateway`] / [`GatewayToAgent`].
//! - viewer <-> gateway probe socket: [`ProbeCtl`] / [`ProbeEvt`].
//!
//! Every enum has an `Unknown` catch-all: receivers must ignore unknown `t` values.
//! Unknown fields are ignored as well (serde default behaviour), so fields can be added
//! without bumping the protocol version.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// shared types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Perms {
    /// Screen & System Audio Recording granted.
    pub screen: bool,
    /// Accessibility (input injection) granted.
    pub input: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DisplayDesc {
    pub id: u32,
    /// Logical size in points.
    pub w_pt: u32,
    pub h_pt: u32,
    /// Backing scale factor (2.0 on Retina).
    pub scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    #[default]
    Avc,
    Hevc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ViewerInfo {
    #[serde(default)]
    pub ua: String,
    #[serde(default)]
    pub dpr: f32,
    /// CSS pixels [w, h].
    #[serde(default)]
    pub viewport: [u32; 2],
    #[serde(default)]
    pub decoders: DecoderCaps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DecoderCaps {
    #[serde(default)]
    pub avc: bool,
    #[serde(default)]
    pub hevc: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentInfo {
    /// "macos" | "windows"
    pub os: String,
    pub version: String,
    #[serde(default)]
    pub displays: Vec<DisplayDesc>,
}

/// Stream parameters requested by the viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamParams {
    /// Output video size in pixels (agent rounds to even and clamps to the display's native size).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    #[serde(default)]
    pub codec: VideoCodec,
    pub start_kbps: u32,
    pub min_kbps: u32,
    pub max_kbps: u32,
    /// Adaptive bitrate on (true) or fixed at `start_kbps` (false).
    #[serde(default = "default_true")]
    pub adaptive: bool,
    #[serde(default)]
    pub audio: bool,
    /// Display id from the agent's `hello`; 0 = main display.
    #[serde(default)]
    pub display: u32,
}

fn default_true() -> bool {
    true
}

/// Partial update of [`StreamParams`]. Changing size/codec/display starts a new epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamParamsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<VideoCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<u32>,
}

/// Agent state, reported to the gateway and forwarded to viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentState {
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub perms: Perms,
    #[serde(default)]
    pub in_session: bool,
}

/// Once-per-second agent statistics for the viewer HUD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentStats {
    pub target_kbps: u32,
    /// Encoded bitrate over the last second.
    pub enc_kbps: u32,
    /// Captured frames per second (before the window gate).
    pub cap_fps: f32,
    /// Encoded (sent) frames per second.
    pub enc_fps: f32,
    /// Frames dropped before encode by the flow window, last second.
    pub drop_window: u32,
    /// Encoded frames discarded after encode (should stay 0), cumulative.
    pub drop_post: u32,
    pub enc_ms_p50: f32,
    pub enc_ms_p99: f32,
    pub rtt_min_ms: f32,
    /// Estimated queueing delay (bufferbloat) in the path, ms.
    pub q_ms: f32,
    pub inflight_frames: u32,
    pub inflight_bytes: u32,
    pub window_frames: u32,
    pub window_bytes: u32,
}

// ---------------------------------------------------------------------------
// viewer <-> agent

/// Viewer -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ViewerMsg {
    Hello {
        proto: u16,
        min_proto: u16,
        #[serde(default)]
        viewer: ViewerInfo,
    },
    Start(StreamParams),
    Set(StreamParamsPatch),
    KeyframeReq {
        #[serde(default)]
        reason: String,
    },
    Pause,
    Resume,
    #[serde(other)]
    Unknown,
}

/// Agent -> viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    Hello {
        proto: u16,
        agent: AgentInfo,
        #[serde(default)]
        caps: Vec<String>,
    },
    /// Sent before the first VIDEO frame of every epoch.
    VideoConfig {
        epoch: u16,
        /// WebCodecs codec string, e.g. "avc1.640033".
        codec: String,
        width: u32,
        height: u32,
        /// base64 (standard, padded) avcC / hvcC record -> `VideoDecoderConfig.description`.
        description: String,
    },
    AudioConfig {
        codec: String,
        sample_rate: u32,
        channels: u32,
    },
    Stats(AgentStats),
    State(AgentState),
    Error {
        /// tcc_screen | tcc_input | capture_stopped | display_asleep | proto_mismatch | bad_request | internal
        code: String,
        #[serde(default)]
        msg: String,
    },
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// gateway -> viewer (same socket as AgentMsg)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum GatewayToViewer {
    /// Sent once the agent's session connection has been joined; the viewer then sends `hello`.
    Session {
        machine: String,
        session: String,
        #[serde(default)]
        agent: AgentState,
    },
    GwError {
        code: String,
        #[serde(default)]
        msg: String,
    },
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// agent <-> gateway (agent control socket)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentToGateway {
    Register {
        machine: String,
        agent_version: String,
        os: String,
        gw_proto: u16,
        #[serde(default)]
        state: AgentState,
    },
    State(AgentState),
    SessionEnd {
        session: String,
        #[serde(default)]
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum GatewayToAgent {
    Registered {
        gw_proto: u16,
    },
    /// Agent must dial `/ws/agent/session/<session>` within 10 s.
    SessionRequest {
        session: String,
        #[serde(default)]
        user: String,
        #[serde(default)]
        client_ip: String,
    },
    SessionEnd {
        session: String,
        #[serde(default)]
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// viewer <-> gateway network probe (/ws/probe)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeTest {
    /// Gateway streams synthetic VIDEO frames (random payload) at `kbps`/`fps`; viewer ACKs on receipt.
    Down,
    /// Viewer streams PROBE_UP frames at `kbps`; gateway reports totals.
    Up,
    /// No payload traffic; only optional pings. Measures idle survival.
    Idle,
}

/// Viewer -> gateway on the probe socket. Binary PING frames are answered with PONG at any time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ProbeCtl {
    Run {
        test: ProbeTest,
        #[serde(default)]
        kbps: u32,
        #[serde(default = "default_fps")]
        fps: u32,
        seconds: u32,
        /// Down test: gate frames with the real ACK window (rd-flow) instead of a fixed rate.
        #[serde(default)]
        windowed: bool,
    },
    Stop,
    #[serde(other)]
    Unknown,
}

fn default_fps() -> u32 {
    60
}

/// Gateway -> viewer on the probe socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ProbeEvt {
    /// First message on connect.
    Hello {
        server_time_us: u64,
        #[serde(default)]
        client_ip: String,
        #[serde(default)]
        user: String,
    },
    /// A down/up/idle run finished (or was stopped).
    Done {
        test: ProbeTest,
        elapsed_ms: u64,
        /// Down: frames/bytes sent by the gateway. Up: frames/bytes received by the gateway.
        frames: u64,
        bytes: u64,
        #[serde(default)]
        window_drops: u64,
        #[serde(default)]
        stopped: bool,
    },
    Error {
        code: String,
        #[serde(default)]
        msg: String,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_msgs() {
        let m: ViewerMsg = serde_json::from_str(
            r#"{"t":"start","width":1728,"height":1118,"fps":60,"codec":"avc","start_kbps":8000,"min_kbps":1000,"max_kbps":20000}"#,
        )
        .unwrap();
        match m {
            ViewerMsg::Start(p) => {
                assert_eq!(p.width, 1728);
                assert!(p.adaptive);
                assert!(!p.audio);
                assert_eq!(p.display, 0);
            }
            other => panic!("{other:?}"),
        }
        let s = serde_json::to_string(&ViewerMsg::KeyframeReq {
            reason: "resume".into(),
        })
        .unwrap();
        assert_eq!(s, r#"{"t":"keyframe_req","reason":"resume"}"#);
        assert_eq!(
            serde_json::to_string(&ViewerMsg::Pause).unwrap(),
            r#"{"t":"pause"}"#
        );
        let u: ViewerMsg = serde_json::from_str(r#"{"t":"from_the_future","x":1}"#).unwrap();
        assert_eq!(u, ViewerMsg::Unknown);
        let set: ViewerMsg = serde_json::from_str(r#"{"t":"set","max_kbps":5000}"#).unwrap();
        assert_eq!(
            set,
            ViewerMsg::Set(StreamParamsPatch {
                max_kbps: Some(5000),
                ..Default::default()
            })
        );
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"t":"set","max_kbps":5000}"#
        );
    }

    #[test]
    fn agent_msgs() {
        let m = AgentMsg::VideoConfig {
            epoch: 1,
            codec: "avc1.640033".into(),
            width: 1728,
            height: 1118,
            description: "AWQAM/8=".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with(r#"{"t":"video_config","epoch":1"#), "{s}");
        assert_eq!(serde_json::from_str::<AgentMsg>(&s).unwrap(), m);
        let st = serde_json::to_string(&AgentMsg::State(AgentState {
            locked: true,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(
            st,
            r#"{"t":"state","locked":true,"perms":{"screen":false,"input":false},"in_session":false}"#
        );
        let stats = serde_json::to_string(&AgentMsg::Stats(AgentStats::default())).unwrap();
        assert!(
            stats.starts_with(r#"{"t":"stats","target_kbps":0"#),
            "{stats}"
        );
    }

    #[test]
    fn gateway_msgs() {
        let r: GatewayToAgent = serde_json::from_str(
            r#"{"t":"session_request","session":"abc","user":"me@x","client_ip":"1.2.3.4"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            GatewayToAgent::SessionRequest {
                session: "abc".into(),
                user: "me@x".into(),
                client_ip: "1.2.3.4".into()
            }
        );
        let v = serde_json::to_string(&GatewayToViewer::Session {
            machine: "mbp".into(),
            session: "abc".into(),
            agent: AgentState::default(),
        })
        .unwrap();
        assert!(v.starts_with(r#"{"t":"session","machine":"mbp""#));
        let e: GatewayToViewer =
            serde_json::from_str(r#"{"t":"gw_error","code":"agent_offline"}"#).unwrap();
        assert_eq!(
            e,
            GatewayToViewer::GwError {
                code: "agent_offline".into(),
                msg: String::new()
            }
        );
        let reg = serde_json::to_string(&AgentToGateway::Register {
            machine: "mbp".into(),
            agent_version: "0.1.0".into(),
            os: "macOS 26.3".into(),
            gw_proto: 1,
            state: AgentState::default(),
        })
        .unwrap();
        assert!(reg.starts_with(r#"{"t":"register","machine":"mbp""#));
    }

    #[test]
    fn probe_msgs() {
        let r: ProbeCtl =
            serde_json::from_str(r#"{"t":"run","test":"down","kbps":10000,"seconds":20}"#).unwrap();
        assert_eq!(
            r,
            ProbeCtl::Run {
                test: ProbeTest::Down,
                kbps: 10000,
                fps: 60,
                seconds: 20,
                windowed: false
            }
        );
        let d = serde_json::to_string(&ProbeEvt::Done {
            test: ProbeTest::Up,
            elapsed_ms: 1,
            frames: 2,
            bytes: 3,
            window_drops: 0,
            stopped: false,
        })
        .unwrap();
        assert!(d.starts_with(r#"{"t":"done","test":"up""#));
    }
}
