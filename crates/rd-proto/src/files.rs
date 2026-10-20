//! File channel messages (docs/files.md): JSON text frames between viewer and agent on a file
//! channel (`/ws/files/{machine}` ⇄ `/ws/agent/files/{channel}`), tagged by `"t"`. The bytes
//! travel as binary FILE_DATA / FILE_ACK frames ([`crate::bin`]).
//!
//! Like every other channel: unknown `t` values parse as `Unknown`, unknown fields are ignored.

use serde::{Deserialize, Serialize};

use crate::control::AgentState;

/// File channel protocol version spoken by this build.
pub const FILES_PROTO_VERSION: u16 = 1;

/// Largest message on a file channel, either way (a FILE_DATA frame is at most
/// [`MAX_CHUNK`] + its header; a large `list` reply is the biggest JSON).
pub const MAX_MESSAGE: usize = 8 << 20;

/// Most bytes one FILE_DATA frame carries.
pub const MAX_CHUNK: usize = 256 << 10;

/// Most entries a `list` reply carries (`truncated: true` beyond).
pub const MAX_LIST_ENTRIES: usize = 20_000;

/// Capabilities an agent lists in its file `hello`.
pub mod caps {
    /// `delete` can move to the Trash / Recycle Bin.
    pub const TRASH: &str = "trash";
    /// Folder (and multi-item) downloads as a zip archive.
    pub const ZIP: &str = "zip";
    /// Uploads can resume (`write` with `resume: true`).
    pub const RESUME: &str = "resume";
}

/// Error codes of [`FilesAgentMsg::Err`].
pub mod err {
    pub const NOT_FOUND: &str = "not_found";
    pub const EXISTS: &str = "exists";
    pub const DENIED: &str = "denied";
    pub const NOT_DIR: &str = "not_dir";
    pub const IS_DIR: &str = "is_dir";
    pub const BAD_PATH: &str = "bad_path";
    pub const BAD_NAME: &str = "bad_name";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const CHANGED: &str = "changed";
    pub const NO_SPACE: &str = "no_space";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const PROTO_MISMATCH: &str = "proto_mismatch";
    pub const IO: &str = "io";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    #[default]
    File,
    Dir,
    /// Devices, sockets, broken links, ...
    #[serde(other)]
    Other,
}

/// One file or folder, as `list` and `stat` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Entry {
    pub name: String,
    /// After following a symbolic link (`link`); a broken link is `Other`.
    pub kind: EntryKind,
    #[serde(default)]
    pub size: u64,
    /// Last modification, Unix ms (0 when unknown).
    #[serde(default)]
    pub mtime_ms: u64,
    /// A dot name, or the hidden attribute on Windows.
    #[serde(default)]
    pub hidden: bool,
    /// A symbolic link (`kind` describes its target).
    #[serde(default)]
    pub link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlaceKind {
    Home,
    Desktop,
    Documents,
    Downloads,
    /// A Windows drive letter, or the macOS root volume.
    Drive,
    /// Another mounted volume.
    Volume,
    #[default]
    #[serde(other)]
    Other,
}

/// A starting point offered in the file browser's sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Place {
    pub kind: PlaceKind,
    pub name: String,
    pub path: String,
}

/// What an upload does when its target exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    /// Answer `exists`.
    #[default]
    Fail,
    /// Replace an existing file.
    Replace,
    /// Keep both: the upload gets the first free `name (2).ext`.
    Rename,
}

/// `read` with `expect`: the file must still have this size and time (a resumed download).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Expect {
    pub size: u64,
    pub mtime_ms: u64,
}

/// Viewer -> agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FilesViewerMsg {
    Hello {
        proto: u16,
    },
    List {
        id: u32,
        path: String,
    },
    Stat {
        id: u32,
        path: String,
    },
    Mkdir {
        id: u32,
        path: String,
        #[serde(default)]
        exist_ok: bool,
    },
    Rename {
        id: u32,
        from: String,
        to: String,
    },
    Delete {
        id: u32,
        paths: Vec<String>,
        #[serde(default)]
        trash: bool,
    },
    /// A download: one file from `offset`, or with `zip` the `paths` as one archive.
    Read {
        id: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        path: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        zip: bool,
        /// File name of the archive (zip only).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<Expect>,
    },
    /// An upload; its FILE_DATA frames follow right away (after the agent's offset answer when
    /// `resume`).
    Write {
        id: u32,
        path: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime_ms: Option<u64>,
        #[serde(default)]
        conflict: Conflict,
        /// Names the temporary file, so an interrupted upload can resume.
        token: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        resume: bool,
    },
    Cancel {
        id: u32,
    },
    #[serde(other)]
    Unknown,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Agent -> viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FilesAgentMsg {
    Hello {
        proto: u16,
        /// "macos" | "windows"
        os: String,
        /// Path separator: "/" or "\\".
        sep: String,
        home: String,
        #[serde(default)]
        places: Vec<Place>,
        #[serde(default)]
        caps: Vec<String>,
        #[serde(default)]
        state: AgentState,
    },
    /// The agent's state changed (`in_session` sets the transfer target delay).
    State(AgentState),
    Ok {
        id: u32,
    },
    Err {
        id: u32,
        code: String,
        #[serde(default)]
        msg: String,
    },
    List {
        id: u32,
        path: String,
        #[serde(default)]
        parent: Option<String>,
        entries: Vec<Entry>,
        #[serde(default)]
        truncated: bool,
    },
    Stat {
        id: u32,
        entry: Option<Entry>,
    },
    /// Start of a download: a file (`size`, `mtime_ms`) or a zip (`total`, `files`).
    Read {
        id: u32,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<u64>,
    },
    /// Zip downloads: source bytes archived so far.
    Progress {
        id: u32,
        done: u64,
    },
    /// Zip downloads: files left out because they could not be read (archive paths).
    Skipped {
        id: u32,
        paths: Vec<String>,
    },
    /// Answer to a `write` with `resume`: send from this offset.
    Write {
        id: u32,
        offset: u64,
    },
    /// An upload is in place, at `path`.
    Done {
        id: u32,
        path: String,
        size: u64,
    },
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_messages() {
        let r: FilesViewerMsg =
            serde_json::from_str(r#"{"t":"read","id":6,"path":"/a.txt"}"#).unwrap();
        assert_eq!(
            r,
            FilesViewerMsg::Read {
                id: 6,
                path: "/a.txt".into(),
                paths: vec![],
                offset: 0,
                zip: false,
                name: String::new(),
                expect: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"t":"read","id":6,"path":"/a.txt"}"#
        );
        let w: FilesViewerMsg = serde_json::from_str(
            r#"{"t":"write","id":9,"path":"/x/r.pdf","size":3,"token":"ab","conflict":"rename"}"#,
        )
        .unwrap();
        match w {
            FilesViewerMsg::Write {
                conflict, resume, ..
            } => {
                assert_eq!(conflict, Conflict::Rename);
                assert!(!resume);
            }
            other => panic!("{other:?}"),
        }
        let d: FilesViewerMsg =
            serde_json::from_str(r#"{"t":"delete","id":5,"paths":["/b"]}"#).unwrap();
        assert_eq!(
            d,
            FilesViewerMsg::Delete {
                id: 5,
                paths: vec!["/b".into()],
                trash: false
            }
        );
        let u: FilesViewerMsg = serde_json::from_str(r#"{"t":"copy","id":1}"#).unwrap();
        assert_eq!(u, FilesViewerMsg::Unknown);
    }

    #[test]
    fn agent_messages() {
        let list = FilesAgentMsg::List {
            id: 1,
            path: "/Users/me".into(),
            parent: Some("/Users".into()),
            entries: vec![Entry {
                name: "a.txt".into(),
                kind: EntryKind::File,
                size: 1234,
                mtime_ms: 1_729_160_000_000,
                hidden: false,
                link: false,
            }],
            truncated: false,
        };
        let s = serde_json::to_string(&list).unwrap();
        assert_eq!(
            s,
            r#"{"t":"list","id":1,"path":"/Users/me","parent":"/Users","entries":[{"name":"a.txt","kind":"file","size":1234,"mtime_ms":1729160000000,"hidden":false,"link":false}],"truncated":false}"#
        );
        assert_eq!(serde_json::from_str::<FilesAgentMsg>(&s).unwrap(), list);
        let read = FilesAgentMsg::Read {
            id: 8,
            name: "Photos.zip".into(),
            size: None,
            mtime_ms: None,
            total: Some(10),
            files: Some(2),
        };
        assert_eq!(
            serde_json::to_string(&read).unwrap(),
            r#"{"t":"read","id":8,"name":"Photos.zip","total":10,"files":2}"#
        );
        let e: Entry = serde_json::from_str(r#"{"name":"dev","kind":"socket","size":0}"#).unwrap();
        assert_eq!(e.kind, EntryKind::Other);
        let p: Place = serde_json::from_str(r#"{"kind":"cloud","name":"x","path":"/x"}"#).unwrap();
        assert_eq!(p.kind, PlaceKind::Other);
        let st = serde_json::to_string(&FilesAgentMsg::State(AgentState::default())).unwrap();
        assert!(st.starts_with(r#"{"t":"state","locked":false"#), "{st}");
    }
}
