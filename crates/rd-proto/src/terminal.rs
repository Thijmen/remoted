//! Terminal channel messages (docs/terminal.md): JSON text frames between viewer and agent on a
//! terminal channel (`/ws/terminal/{machine}` ⇄ `/ws/agent/terminal/{channel}`), tagged by
//! `"t"`. The shell's output and input travel as binary TERM_OUT / TERM_ACK / TERM_IN frames
//! ([`crate::bin`]).
//!
//! Like every other channel: unknown `t` values parse as `Unknown`, unknown fields are ignored.

use serde::{Deserialize, Serialize};

use crate::control::AgentState;

/// Terminal channel protocol version spoken by this build.
pub const TERMINAL_PROTO_VERSION: u16 = 1;

/// Largest message on a terminal channel, either way.
pub const MAX_MESSAGE: usize = 1 << 20;

/// Most output bytes one TERM_OUT frame carries.
pub const MAX_OUT_CHUNK: usize = 64 << 10;

/// Most input bytes one TERM_IN frame carries (the viewer splits a large paste).
pub const MAX_IN_CHUNK: usize = 64 << 10;

/// Output the agent keeps per shell for replay after a reconnect.
pub const HISTORY_BYTES: usize = 1 << 20;

/// Terminal sizes are clamped to this range (columns and rows).
pub const MIN_SIZE: u16 = 2;
pub const MAX_SIZE: u16 = 1000;

/// Error codes of [`TermAgentMsg::Err`].
pub mod err {
    /// `attach` names a shell the agent does not have (it exited, was closed, timed out after
    /// its detach, or the agent restarted).
    pub const NOT_FOUND: &str = "not_found";
    /// The agent already runs its maximum number of shells.
    pub const BUSY: &str = "busy";
    /// The shell could not be started (`msg` says why).
    pub const SPAWN: &str = "spawn";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const PROTO_MISMATCH: &str = "proto_mismatch";
}

/// Why the agent detaches a viewer from its shell.
pub mod detach {
    /// The same shell was attached from another channel (another tab or browser).
    pub const TAKEN_OVER: &str = "taken_over";
}

/// Viewer -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermViewerMsg {
    /// First message. Without `attach` the agent starts a new shell of `cols` × `rows`; with
    /// `attach` it reattaches to that shell and replays its output from `offset`.
    Hello {
        proto: u16,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach: Option<String>,
        /// Output bytes of the attached shell the viewer already shows.
        #[serde(default)]
        offset: u64,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// End the shell (the user closed it). The agent hangs it up and answers `exit`.
    Close,
    #[serde(other)]
    Unknown,
}

/// Agent -> viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermAgentMsg {
    /// Answer to `hello`. TERM_OUT frames follow from `offset`.
    Hello {
        proto: u16,
        /// The shell's id (32 hex), for `attach` after a reconnect.
        id: String,
        /// "macos" | "windows"
        os: String,
        /// What runs in the terminal, for the title (e.g. "zsh", "pwsh").
        shell: String,
        /// Where the replay starts. Equal to the viewer's `offset` when the agent still has
        /// everything after it; otherwise the oldest output it kept, and `reset` is set.
        offset: u64,
        /// The viewer must clear its screen before showing the replay (a new shell, or output
        /// between the viewer's `offset` and this one is lost).
        reset: bool,
        /// Where the replay ends: output the shell printed before this attach. Output from
        /// here on is live. 0 for a new shell.
        #[serde(default)]
        end: u64,
        state: AgentState,
    },
    State(AgentState),
    /// The shell ended. Sent after its last output; the agent then closes the channel (1000).
    Exit {
        /// Exit status, when the process exited normally.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        /// Unix: the signal that ended it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    /// Another channel attached this shell; the agent closes this one (4001).
    Detached {
        reason: String,
    },
    Err {
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
    fn wire_format() {
        let hello: TermViewerMsg =
            serde_json::from_str(r#"{"t":"hello","proto":1,"cols":80,"rows":24,"x":1}"#).unwrap();
        assert_eq!(
            hello,
            TermViewerMsg::Hello {
                proto: 1,
                cols: 80,
                rows: 24,
                attach: None,
                offset: 0
            }
        );
        let attach = TermViewerMsg::Hello {
            proto: 1,
            cols: 100,
            rows: 30,
            attach: Some("ab".into()),
            offset: 5,
        };
        assert_eq!(
            serde_json::to_string(&attach).unwrap(),
            r#"{"t":"hello","proto":1,"cols":100,"rows":30,"attach":"ab","offset":5}"#
        );
        assert_eq!(
            serde_json::from_str::<TermViewerMsg>(r#"{"t":"later"}"#).unwrap(),
            TermViewerMsg::Unknown
        );
        assert_eq!(
            serde_json::to_string(&TermAgentMsg::Exit {
                code: Some(0),
                signal: None
            })
            .unwrap(),
            r#"{"t":"exit","code":0}"#
        );
        let state = serde_json::to_string(&TermAgentMsg::State(AgentState::default())).unwrap();
        assert!(
            state.starts_with(r#"{"t":"state","locked":false"#),
            "{state}"
        );
    }
}
