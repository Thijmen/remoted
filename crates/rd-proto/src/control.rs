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

/// Whether a secure-desktop helper can show the Windows lock screen or UAC prompt in the
/// viewer. Only Windows with the secure-desktop service installed reports anything but
/// `unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecureState {
    #[default]
    Unavailable,
    Starting,
    Ready,
    Failed,
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
    /// The secure-desktop helper (docs/setup-windows.md). Older agents omit it.
    #[serde(default)]
    pub secure: SecureState,
    /// Windows: the agent's Remote Desktop session is disconnected. Windows draws nothing for
    /// it, so there is no lock screen or desktop to show until someone reconnects. Older
    /// agents omit it.
    #[serde(default)]
    pub disconnected: bool,
}

/// What an agent knows about updates of itself (docs/updates.md), reported to the gateway with
/// `update_status` after `register` and whenever it changes. Added in rd-agent 1.5.0; older
/// gateways ignore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdateStatus {
    /// A newer version from the update manifest; `None` when up to date, not checked yet, or
    /// checks are off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<AvailableUpdate>,
    /// The agent can install updates itself (it was installed the normal way). When false,
    /// `cannot_install` says why, for the portal.
    #[serde(default)]
    pub can_install: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cannot_install: String,
    /// What the agent is doing about updates right now.
    #[serde(default)]
    pub phase: UpdatePhase,
    /// Why the last check or update request failed (empty: it did not).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Last successful check, Unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_ms: Option<u64>,
    /// Outcome of the last update attempt (kept until the next one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<UpdateOutcome>,
}

/// A newer agent version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AvailableUpdate {
    pub version: String,
    /// Release notes of every version newer than the running one, newest first, as Markdown
    /// (at most 16 KiB; cut at a version boundary).
    #[serde(default)]
    pub notes: String,
    /// The release page of `version`.
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    Downloading,
    /// The update helper took over; the agent restarts shortly.
    Installing,
    #[serde(other)]
    Unknown,
}

/// How an update attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdateOutcome {
    pub from: String,
    pub to: String,
    pub result: UpdateResult,
    /// What went wrong (empty when `result` is `ok`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// When it ended, Unix ms.
    #[serde(default)]
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateResult {
    /// The new version is running.
    #[default]
    Ok,
    /// The new version did not come back in time; the previous one was put back.
    RolledBack,
    /// Nothing was installed (download, signature or install failed before the switch).
    Failed,
    #[serde(other)]
    Unknown,
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
    /// Encode-latency term of the flow window's horizon (`rtt_min + enc + 40 ms`): median of
    /// recent encode times, clamped. Added after v1 shipped: older agents omit it (0).
    #[serde(default)]
    pub window_enc_ms: f32,
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
    /// Ctrl+Alt+Del on the Windows secure desktop (the helper calls `SendSAS`).
    Sas,
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

/// Capabilities in `register.caps`.
pub mod agent_caps {
    /// File channels (`files_request`, docs/files.md).
    pub const FILES: &str = "files";
    /// Terminal channels (`terminal_request`, docs/terminal.md). Not listed when the terminal
    /// is turned off in the agent's config or the OS has no pseudo console.
    pub const TERMINAL: &str = "terminal";
    /// Unicode text input (`InputEvent::Text`): the viewer may send committed IME/soft-keyboard
    /// text. Old agents do not list it, and the viewer hides its keyboard button then.
    pub const TEXT: &str = "text";
}

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
        /// What the agent supports beyond screen sessions (see [`agent_caps`]). Added after
        /// gw_proto 1 shipped; older agents omit it, older gateways ignore it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caps: Vec<String>,
    },
    State(AgentState),
    SessionEnd {
        session: String,
        #[serde(default)]
        reason: String,
    },
    /// Sent after `register` and whenever it changes (rd-agent 1.5.0 and later).
    UpdateStatus(UpdateStatus),
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
    /// A viewer opened a file channel (docs/files.md): dial `/ws/agent/files/<channel>` within
    /// 10 s. Sent only to agents with the `files` capability.
    FilesRequest {
        channel: String,
        #[serde(default)]
        user: String,
        #[serde(default)]
        client_ip: String,
    },
    /// A viewer opened a terminal channel (docs/terminal.md): dial
    /// `/ws/agent/terminal/<channel>` within 10 s. Sent only to agents with the `terminal`
    /// capability.
    TerminalRequest {
        channel: String,
        #[serde(default)]
        user: String,
        #[serde(default)]
        client_ip: String,
    },
    /// Install `version` now (the portal's Update button). The agent acts only if it is the
    /// version its own last check found; otherwise it reports an error in `update_status`.
    Update {
        version: String,
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
        assert_eq!(
            serde_json::to_string(&ViewerMsg::Sas).unwrap(),
            r#"{"t":"sas"}"#
        );
        assert_eq!(
            serde_json::from_str::<ViewerMsg>(r#"{"t":"sas"}"#).unwrap(),
            ViewerMsg::Sas
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
            r#"{"t":"state","locked":true,"perms":{"screen":false,"input":false},"in_session":false,"secure":"unavailable","disconnected":false}"#
        );
        let stats = serde_json::to_string(&AgentMsg::Stats(AgentStats::default())).unwrap();
        assert!(
            stats.starts_with(r#"{"t":"stats","target_kbps":0"#),
            "{stats}"
        );
        // Stats from an agent without `window_enc_ms` still parse.
        let old = r#"{"t":"stats","target_kbps":8000,"enc_kbps":1,"cap_fps":60.0,"enc_fps":60.0,"drop_window":0,"drop_post":0,"enc_ms_p50":4.0,"enc_ms_p99":9.0,"rtt_min_ms":20.0,"q_ms":1.0,"inflight_frames":2,"inflight_bytes":9,"window_frames":7,"window_bytes":90000}"#;
        match serde_json::from_str::<AgentMsg>(old).unwrap() {
            AgentMsg::Stats(s) => assert_eq!((s.window_frames, s.window_enc_ms), (7, 0.0)),
            other => panic!("{other:?}"),
        }
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
            caps: vec![],
        })
        .unwrap();
        assert!(reg.starts_with(r#"{"t":"register","machine":"mbp""#));
        assert!(!reg.contains("caps"), "{reg}");
        let reg: AgentToGateway = serde_json::from_str(
            r#"{"t":"register","machine":"mbp","agent_version":"1.6.0","os":"macOS","gw_proto":1,"caps":["files","x"]}"#,
        )
        .unwrap();
        match reg {
            AgentToGateway::Register { caps, .. } => {
                assert!(caps.iter().any(|c| c == agent_caps::FILES))
            }
            other => panic!("{other:?}"),
        }
        let files = serde_json::to_string(&GatewayToAgent::FilesRequest {
            channel: "c1".into(),
            user: "me@x".into(),
            client_ip: "1.2.3.4".into(),
        })
        .unwrap();
        assert_eq!(
            files,
            r#"{"t":"files_request","channel":"c1","user":"me@x","client_ip":"1.2.3.4"}"#
        );
        let term = serde_json::to_string(&GatewayToAgent::TerminalRequest {
            channel: "c2".into(),
            user: "me@x".into(),
            client_ip: "1.2.3.4".into(),
        })
        .unwrap();
        assert_eq!(
            term,
            r#"{"t":"terminal_request","channel":"c2","user":"me@x","client_ip":"1.2.3.4"}"#
        );
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

    #[test]
    fn update_messages() {
        // An agent that has never checked: only the defaults that are always sent.
        let idle = AgentToGateway::UpdateStatus(UpdateStatus::default());
        assert_eq!(
            serde_json::to_string(&idle).unwrap(),
            r#"{"t":"update_status","can_install":false,"phase":"idle"}"#
        );

        let status = UpdateStatus {
            available: Some(AvailableUpdate {
                version: "1.6.0".into(),
                notes: "## 1.6.0\n\n- Faster.".into(),
                url: "https://github.com/Thijmen/remoted/releases/tag/agent-v1.6.0".into(),
            }),
            can_install: true,
            phase: UpdatePhase::Downloading,
            checked_ms: Some(1),
            last: Some(UpdateOutcome {
                from: "1.4.2".into(),
                to: "1.5.0".into(),
                result: UpdateResult::RolledBack,
                error: "1.5.0 did not reconnect within 120 s".into(),
                at_ms: 2,
            }),
            ..Default::default()
        };
        let text = serde_json::to_string(&AgentToGateway::UpdateStatus(status.clone())).unwrap();
        assert!(
            text.starts_with(r#"{"t":"update_status","available":{"version":"1.6.0""#),
            "{text}"
        );
        assert!(text.contains(r#""result":"rolled_back""#), "{text}");
        assert_eq!(
            serde_json::from_str::<AgentToGateway>(&text).unwrap(),
            AgentToGateway::UpdateStatus(status)
        );

        // Newer phases and results from a future agent do not break an older gateway.
        let future: AgentToGateway = serde_json::from_str(
            r#"{"t":"update_status","phase":"verifying","last":{"from":"1","to":"2","result":"partial"},"x":1}"#,
        )
        .unwrap();
        let AgentToGateway::UpdateStatus(future) = future else {
            panic!("not update_status: {future:?}")
        };
        assert_eq!(future.phase, UpdatePhase::Unknown);
        assert_eq!(future.last.unwrap().result, UpdateResult::Unknown);

        let update = GatewayToAgent::Update {
            version: "1.6.0".into(),
        };
        let text = serde_json::to_string(&update).unwrap();
        assert_eq!(text, r#"{"t":"update","version":"1.6.0"}"#);
        assert_eq!(
            serde_json::from_str::<GatewayToAgent>(&text).unwrap(),
            update
        );
    }
}
