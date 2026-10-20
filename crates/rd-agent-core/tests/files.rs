//! File channels end to end (docs/files.md): the agent runtime against a fake gateway, with the
//! test playing the viewer on the file socket. Everything happens in temporary folders.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;

use rd_agent_core::runtime::Timings;
use rd_proto::bin::{self, BinMsg, FileAck, FileDataHeader, Ping, file_flags};
use serde_json::{Value, json};

const CHANNEL_A: &str = "f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1";
const CHANNEL_B: &str = "f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2";

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rd-files-it-{tag}-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The viewer end of a file channel.
struct Files {
    ws: Ws,
    /// FILE_DATA seq of the viewer's own frames.
    seq: u32,
    /// Highest FILE_ACK from the agent.
    acked: u32,
    /// JSON that arrived while waiting for something else.
    pending: Vec<Value>,
}

/// A finished download.
#[derive(Debug)]
struct Download {
    header: Value,
    data: Vec<u8>,
    json: Vec<Value>,
}

impl Files {
    async fn open(setup: &mut Setup, channel: &str) -> Files {
        send_json(
            &mut setup.control.ws,
            json!({"t": "files_request", "channel": channel, "user": "me@test", "client_ip": "127.0.0.1"}),
        )
        .await;
        let conn = setup.gw.session().await;
        assert_eq!(conn.path, format!("/ws/agent/files/{channel}"));
        assert_eq!(conn.auth.as_deref(), Some(&*format!("Bearer {TOKEN}")));
        Files {
            ws: conn.ws,
            seq: 0,
            acked: 0,
            pending: Vec::new(),
        }
    }

    async fn hello(&mut self) -> Value {
        self.send(json!({"t": "hello", "proto": 1})).await;
        self.expect(|v| v["t"] == "hello").await
    }

    async fn send(&mut self, v: Value) {
        send_json(&mut self.ws, v).await;
    }

    /// Next message; FILE_ACKs are recorded, PONGs returned as JSON `{"t":"pong"}`.
    async fn next(&mut self) -> Msg {
        loop {
            let msg = recv(&mut self.ws).await;
            if let Msg::Bin(b) = &msg {
                match bin::parse(b) {
                    Ok(BinMsg::FileAck(a)) => {
                        self.acked = a.seq;
                        continue;
                    }
                    Ok(BinMsg::Pong(p)) => return Msg::Json(json!({"t": "pong", "id": p.id})),
                    _ => {}
                }
            }
            return msg;
        }
    }

    /// The next JSON message matching `pred` (others are kept for later, `state` dropped).
    async fn expect(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.pending.iter().position(&pred) {
            return self.pending.remove(i);
        }
        loop {
            match self.next().await {
                Msg::Json(v) if pred(&v) => return v,
                Msg::Json(v) if v["t"] == "state" => {}
                Msg::Json(v) => self.pending.push(v),
                Msg::Bin(_) => panic!("unexpected binary message"),
                Msg::Close(code) => panic!("file channel closed ({code:?})"),
            }
        }
    }

    /// The reply to request `id`.
    async fn reply(&mut self, id: u32) -> Value {
        self.expect(|v| v["id"] == id && v["t"] != "state").await
    }

    /// Waits until the agent acknowledged every frame sent so far.
    async fn wait_acked(&mut self) {
        while self.acked != self.seq {
            match recv(&mut self.ws).await {
                Msg::Bin(b) => match bin::parse(&b) {
                    Ok(BinMsg::FileAck(a)) => self.acked = a.seq,
                    other => panic!("unexpected binary message {other:?}"),
                },
                Msg::Json(v) => self.pending.push(v),
                Msg::Close(code) => panic!("file channel closed ({code:?})"),
            }
        }
    }

    async fn request(&mut self, mut v: Value, id: u32) -> Value {
        v["id"] = id.into();
        self.send(v).await;
        self.reply(id).await
    }

    async fn data(&mut self, id: u32, offset: u64, bytes: &[u8], end: bool) {
        self.seq += 1;
        let h = FileDataHeader {
            flags: if end { file_flags::END } else { 0 },
            id,
            seq: self.seq,
            offset,
        };
        send_bin(&mut self.ws, h.to_message(bytes)).await;
    }

    /// `write` followed at once by all its data, in chunks of `chunk` bytes.
    async fn upload_frames(&mut self, id: u32, path: &Path, data: &[u8], conflict: &str) {
        self.send(
            json!({"t": "write", "id": id, "path": s(path), "size": data.len(),
                         "mtime_ms": 1_600_000_000_000u64, "conflict": conflict,
                         "token": format!("tok{id}")}),
        )
        .await;
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(7000).collect()
        };
        let last = chunks.len() - 1;
        let mut offset = 0;
        for (i, c) in chunks.into_iter().enumerate() {
            self.data(id, offset, c, i == last).await;
            offset += c.len() as u64;
        }
    }

    async fn upload(&mut self, id: u32, path: &Path, data: &[u8], conflict: &str) -> Value {
        self.upload_frames(id, path, data, conflict).await;
        self.expect(|v| v["id"] == id && (v["t"] == "done" || v["t"] == "err"))
            .await
    }

    /// A download, acknowledging every frame.
    async fn download(&mut self, mut req: Value, id: u32) -> std::result::Result<Download, Value> {
        req["id"] = id.into();
        let start = req["offset"].as_u64().unwrap_or(0);
        self.send(req).await;
        let header = self
            .expect(|v| v["id"] == id && (v["t"] == "read" || v["t"] == "err"))
            .await;
        if header["t"] == "err" {
            return Err(header);
        }
        let mut data = Vec::new();
        let mut json = Vec::new();
        loop {
            match self.next().await {
                Msg::Bin(b) => {
                    let Ok(BinMsg::FileData(h, payload)) = bin::parse(&b) else {
                        panic!("unexpected binary message");
                    };
                    assert_eq!(h.id, id);
                    assert_eq!(h.offset, start + data.len() as u64, "frames in order");
                    data.extend_from_slice(payload);
                    send_bin(&mut self.ws, FileAck { seq: h.seq }.to_message()).await;
                    if h.flags & file_flags::END != 0 {
                        return Ok(Download { header, data, json });
                    }
                }
                Msg::Json(v) if v["t"] == "err" && v["id"] == id => return Err(v),
                Msg::Json(v) if v["id"] == id => json.push(v),
                Msg::Json(v) => self.pending.push(v),
                Msg::Close(code) => panic!("closed during a download ({code:?})"),
            }
        }
    }
}

async fn setup_files() -> (Setup, Files) {
    let mut setup = setup().await;
    let mut f = Files::open(&mut setup, CHANNEL_A).await;
    f.hello().await;
    (setup, f)
}

#[tokio::test]
async fn hello_lists_places_and_caps() {
    let mut setup = setup().await;
    assert_eq!(setup.control.register["caps"], json!(["files"]));
    let mut f = Files::open(&mut setup, CHANNEL_A).await;
    // Requests before hello are refused.
    let early = f.request(json!({"t": "list", "path": "/"}), 1).await;
    assert_eq!(early["code"], "bad_request");
    let hello = f.hello().await;
    assert_eq!(hello["proto"], 1);
    assert_eq!(hello["os"], "fakeos");
    assert_eq!(hello["sep"], std::path::MAIN_SEPARATOR_STR);
    assert_eq!(hello["caps"], json!(["trash", "zip", "resume"]));
    assert_eq!(hello["state"]["in_session"], false);
    assert!(hello["places"].is_array());
    // A binary PING is answered.
    send_bin(
        &mut f.ws,
        Ping {
            id: 77,
            t_origin_us: 5,
        }
        .to_message(),
    )
    .await;
    assert_eq!(f.expect(|v| v["t"] == "pong").await["id"], 77);
    setup.agent.stop().await;
}

#[tokio::test]
async fn browse_and_change() {
    let (setup, mut f) = setup_files().await;
    let dir = tempdir("browse");
    fs::write(dir.join("a.txt"), b"hello").unwrap();
    fs::create_dir(dir.join("sub")).unwrap();

    let list = f.request(json!({"t": "list", "path": s(&dir)}), 1).await;
    assert_eq!(list["t"], "list");
    assert_eq!(list["path"], s(&dir));
    assert_eq!(list["parent"], s(dir.parent().unwrap()));
    let mut names: Vec<&str> = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "sub"]);

    let stat = f
        .request(json!({"t": "stat", "path": s(&dir.join("a.txt"))}), 2)
        .await;
    assert_eq!(stat["entry"]["size"], 5);
    let none = f
        .request(json!({"t": "stat", "path": s(&dir.join("nope"))}), 3)
        .await;
    assert_eq!(none["entry"], Value::Null);
    let bad = f.request(json!({"t": "list", "path": "relative"}), 4).await;
    assert_eq!(bad["code"], "bad_path");
    let missing = f
        .request(json!({"t": "list", "path": s(&dir.join("nope"))}), 5)
        .await;
    assert_eq!(missing["code"], "not_found");

    let mk = f
        .request(json!({"t": "mkdir", "path": s(&dir.join("new"))}), 6)
        .await;
    assert_eq!(mk["t"], "ok");
    assert!(dir.join("new").is_dir());
    let again = f
        .request(json!({"t": "mkdir", "path": s(&dir.join("new"))}), 7)
        .await;
    assert_eq!(again["code"], "exists");
    let ok = f
        .request(
            json!({"t": "mkdir", "path": s(&dir.join("new")), "exist_ok": true}),
            8,
        )
        .await;
    assert_eq!(ok["t"], "ok");

    let ren = f
        .request(
            json!({"t": "rename", "from": s(&dir.join("a.txt")), "to": s(&dir.join("sub"))}),
            9,
        )
        .await;
    assert_eq!(ren["code"], "exists");
    let ren = f
        .request(
            json!({"t": "rename", "from": s(&dir.join("a.txt")), "to": s(&dir.join("b.txt"))}),
            10,
        )
        .await;
    assert_eq!(ren["t"], "ok");
    assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"hello");

    let del = f
        .request(
            json!({"t": "delete", "paths": [s(&dir.join("b.txt")), s(&dir.join("sub"))], "trash": true}),
            11,
        )
        .await;
    assert_eq!(del["t"], "ok");
    assert!(!dir.join("b.txt").exists() && !dir.join("sub").exists());
    assert_eq!(setup.platform.count(|e| matches!(e, Event::Trashed(_))), 2);
    // Already gone counts as deleted (a request re-sent after a reconnect).
    let del = f
        .request(
            json!({"t": "delete", "paths": [s(&dir.join("new")), s(&dir.join("gone"))], "trash": false}),
            12,
        )
        .await;
    assert_eq!(del["t"], "ok", "{del}");
    assert!(!dir.join("new").exists());
    let root = f
        .request(json!({"t": "delete", "paths": ["/"], "trash": false}), 13)
        .await;
    assert_eq!(root["code"], "bad_path");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let ro = dir.join("ro");
        fs::create_dir(&ro).unwrap();
        fs::write(ro.join("x"), b"").unwrap();
        fs::write(dir.join("y"), b"").unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let del = f
            .request(
                json!({"t": "delete", "paths": [s(&ro.join("x")), s(&dir.join("y"))], "trash": false}),
                14,
            )
            .await;
        assert_eq!(del["code"], "denied");
        assert!(del["msg"].as_str().unwrap().contains("not deleted"));
        assert!(dir.join("y").exists(), "stopped at the first failure");
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

#[tokio::test]
async fn uploads_pipelined_with_conflicts() {
    let (setup, mut f) = setup_files().await;
    let dir = tempdir("upload");
    // Three files back to back, without waiting for any answer.
    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    f.upload_frames(1, &dir.join("one.txt"), b"first", "fail")
        .await;
    f.upload_frames(2, &dir.join("big.bin"), &big, "fail").await;
    f.upload_frames(3, &dir.join("empty"), b"", "fail").await;
    for id in 1..=3 {
        let done = f.reply(id).await;
        assert_eq!(done["t"], "done", "{done}");
    }
    assert_eq!(fs::read(dir.join("one.txt")).unwrap(), b"first");
    assert_eq!(fs::read(dir.join("big.bin")).unwrap(), big);
    assert_eq!(fs::read(dir.join("empty")).unwrap(), b"");
    let mtime = fs::metadata(dir.join("one.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        1_600_000_000_000
    );
    // Every frame is acknowledged.
    f.wait_acked().await;

    let exists = f.upload(4, &dir.join("one.txt"), b"second", "fail").await;
    assert_eq!(exists["code"], "exists");
    let renamed = f.upload(5, &dir.join("one.txt"), b"second", "rename").await;
    assert_eq!(renamed["path"], s(&dir.join("one (2).txt")));
    let replaced = f.upload(6, &dir.join("one.txt"), b"third", "replace").await;
    assert_eq!(replaced["path"], s(&dir.join("one.txt")));
    assert_eq!(fs::read(dir.join("one.txt")).unwrap(), b"third");
    fs::create_dir(dir.join("folder")).unwrap();
    let is_dir = f.upload(7, &dir.join("folder"), b"x", "replace").await;
    assert_eq!(is_dir["code"], "is_dir");
    let bad = f.upload(8, &dir.join("a/b"), b"x", "fail").await;
    assert_eq!(bad["code"], "not_found", "missing parent folder: {bad}");

    // Two uploads at once, their data interleaved; one cancelled half way.
    f.send(json!({"t": "write", "id": 10, "path": s(&dir.join("x.txt")), "size": 6, "conflict": "fail", "token": "tx"}))
        .await;
    f.send(json!({"t": "write", "id": 11, "path": s(&dir.join("y.txt")), "size": 6, "conflict": "fail", "token": "ty"}))
        .await;
    f.data(10, 0, b"xxx", false).await;
    f.data(11, 0, b"yyy", false).await;
    f.data(10, 3, b"XXX", true).await;
    f.send(json!({"t": "cancel", "id": 11})).await;
    f.data(11, 3, b"YYY", true).await;
    assert_eq!(f.reply(10).await["t"], "done");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(dir.join("x.txt")).unwrap(), b"xxxXXX");
    assert!(!dir.join("y.txt").exists());
    // Data at the wrong offset fails the upload; no temporary files are left.
    f.send(json!({"t": "write", "id": 12, "path": s(&dir.join("z.txt")), "size": 6, "conflict": "fail", "token": "tz"}))
        .await;
    f.data(12, 3, b"zzz", true).await;
    assert_eq!(f.reply(12).await["code"], "bad_request");
    let leftovers: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rdpart"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

#[tokio::test]
async fn an_interrupted_upload_resumes_on_a_new_channel() {
    let timings = Timings {
        upload_resume_keep: Duration::from_millis(1500),
        ..timings()
    };
    let mut setup = setup_with(FakePlatform::new(100), timings).await;
    let dir = tempdir("resume");
    let target = dir.join("movie.bin");
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();

    let mut f = Files::open(&mut setup, CHANNEL_A).await;
    f.hello().await;
    f.send(json!({"t": "write", "id": 1, "path": s(&target), "size": data.len(), "conflict": "fail", "token": "resumeme"}))
        .await;
    f.data(1, 0, &data[..20_000], false).await;
    // Wait for the ack, so the bytes are on disk, then lose the connection.
    f.wait_acked().await;
    drop(f);

    let mut g = Files::open(&mut setup, CHANNEL_B).await;
    g.hello().await;
    // The old channel's worker must have parked the upload; it does so right after the socket
    // is gone.
    tokio::time::sleep(Duration::from_millis(200)).await;
    g.send(json!({"t": "write", "id": 7, "path": s(&target), "size": data.len(), "conflict": "fail", "token": "resumeme", "resume": true}))
        .await;
    let w = g.reply(7).await;
    assert_eq!(w, json!({"t": "write", "id": 7, "offset": 20_000}));
    g.data(7, 20_000, &data[20_000..], true).await;
    assert_eq!(g.reply(7).await["t"], "done");
    assert_eq!(fs::read(&target).unwrap(), data);

    // An unknown token starts from 0.
    g.send(json!({"t": "write", "id": 8, "path": s(&dir.join("other")), "size": 3, "conflict": "fail", "token": "unknown", "resume": true}))
        .await;
    assert_eq!(g.reply(8).await["offset"], 0);
    g.data(8, 0, b"abc", true).await;
    assert_eq!(g.reply(8).await["t"], "done");

    // An interrupted upload nobody resumes is deleted after the keep time.
    g.send(json!({"t": "write", "id": 9, "path": s(&dir.join("lost")), "size": 10, "conflict": "fail", "token": "lost"}))
        .await;
    g.data(9, 0, b"12345", false).await;
    g.wait_acked().await;
    let part = dir.join(".lost.lost.rdpart");
    assert!(part.exists());
    drop(g);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(part.exists(), "kept for resuming");
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(!part.exists(), "deleted after the keep time");
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

#[tokio::test]
async fn downloads() {
    let (setup, mut f) = setup_files().await;
    let dir = tempdir("download");
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 249) as u8).collect();
    fs::write(dir.join("f.bin"), &data).unwrap();
    fs::write(dir.join("empty"), b"").unwrap();

    let d = f
        .download(json!({"t": "read", "path": s(&dir.join("f.bin"))}), 1)
        .await
        .unwrap();
    assert_eq!(d.header["name"], "f.bin");
    assert_eq!(d.header["size"], data.len());
    assert_eq!(d.data, data);
    let mtime = d.header["mtime_ms"].as_u64().unwrap();

    let d = f
        .download(json!({"t": "read", "path": s(&dir.join("empty"))}), 2)
        .await
        .unwrap();
    assert!(d.data.is_empty());

    // Resume from an offset, checking the file did not change.
    let d = f
        .download(
            json!({"t": "read", "path": s(&dir.join("f.bin")), "offset": 600_000,
                   "expect": {"size": data.len(), "mtime_ms": mtime}}),
            3,
        )
        .await
        .unwrap();
    let got = d.data;
    assert_eq!(got, &data[600_000..]);

    let changed = f
        .download(
            json!({"t": "read", "path": s(&dir.join("f.bin")), "offset": 10, "expect": {"size": 5, "mtime_ms": mtime}}),
            4,
        )
        .await
        .unwrap_err();
    assert_eq!(changed["code"], "changed");
    let is_dir = f
        .download(json!({"t": "read", "path": s(&dir)}), 5)
        .await
        .unwrap_err();
    assert_eq!(is_dir["code"], "is_dir");
    let missing = f
        .download(json!({"t": "read", "path": s(&dir.join("nope"))}), 6)
        .await
        .unwrap_err();
    assert_eq!(missing["code"], "not_found");
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

#[tokio::test]
async fn downloads_wait_for_acks_and_can_be_cancelled() {
    let (setup, mut f) = setup_files().await;
    let dir = tempdir("window");
    let data = vec![7u8; 4 << 20];
    fs::write(dir.join("big"), &data).unwrap();
    f.send(json!({"t": "read", "id": 1, "path": s(&dir.join("big"))}))
        .await;
    assert_eq!(f.expect(|v| v["id"] == 1).await["t"], "read");
    // Without acks the agent stops at its window (64 KiB to start with).
    let mut seen = 0usize;
    let mut last_seq = 0;
    while let Ok(msg) = tokio::time::timeout(Duration::from_millis(300), f.next()).await {
        if let Msg::Bin(b) = msg
            && let Ok(BinMsg::FileData(h, p)) = bin::parse(&b)
        {
            seen += p.len();
            last_seq = h.seq;
        }
    }
    assert!(seen > 0 && seen <= 64 << 10, "{seen} bytes before any ack");
    // Acks open the window again.
    send_bin(&mut f.ws, FileAck { seq: last_seq }.to_message()).await;
    let Msg::Bin(b) = f.next().await else {
        panic!("expected more data")
    };
    assert!(matches!(bin::parse(&b), Ok(BinMsg::FileData(..))));
    // Cancel: no END; the next download comes through.
    f.send(json!({"t": "cancel", "id": 1})).await;
    fs::write(dir.join("small"), b"small").unwrap();
    f.send(json!({"t": "read", "id": 2, "path": s(&dir.join("small"))}))
        .await;
    // Ack whatever of download 1 is still on its way, until download 2 starts.
    let header = loop {
        match f.next().await {
            Msg::Bin(b) => {
                let Ok(BinMsg::FileData(h, _)) = bin::parse(&b) else {
                    panic!()
                };
                assert_eq!(h.id, 1);
                assert_eq!(
                    h.flags & file_flags::END,
                    0,
                    "a cancelled download has no END"
                );
                send_bin(&mut f.ws, FileAck { seq: h.seq }.to_message()).await;
            }
            Msg::Json(v) if v["id"] == 2 => break v,
            Msg::Json(_) => {}
            Msg::Close(c) => panic!("closed {c:?}"),
        }
    };
    assert_eq!(header["size"], 5);
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

/// Entries of a zip archive (name, data) from its central directory, with CRC checks left to
/// the unit tests of the writer.
fn zip_entries(zip: &[u8]) -> Vec<(String, Option<Vec<u8>>)> {
    let u16_at = |o: usize| u16::from_le_bytes([zip[o], zip[o + 1]]) as usize;
    let u32_at = |o: usize| u32::from_le_bytes(zip[o..o + 4].try_into().unwrap()) as usize;
    let eocd = zip.len() - 22;
    assert_eq!(u32_at(eocd), 0x0605_4b50);
    let count = u16_at(eocd + 10);
    let mut p = u32_at(eocd + 16);
    let mut out = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(p), 0x0201_4b50);
        let size = u32_at(p + 24);
        let (name_len, extra_len, comment_len) = (u16_at(p + 28), u16_at(p + 30), u16_at(p + 32));
        let offset = u32_at(p + 42);
        let name = String::from_utf8(zip[p + 46..p + 46 + name_len].to_vec()).unwrap();
        let data_at = offset + 30 + u16_at(offset + 26) + u16_at(offset + 28);
        let data = (!name.ends_with('/')).then(|| zip[data_at..data_at + size].to_vec());
        out.push((name, data));
        p += 46 + name_len + extra_len + comment_len;
    }
    out
}

#[tokio::test]
async fn zip_downloads() {
    let (setup, mut f) = setup_files().await;
    let dir = tempdir("zip");
    let photos = dir.join("Photos");
    fs::create_dir_all(photos.join("2024/empty")).unwrap();
    fs::write(photos.join("a.jpg"), vec![1u8; 300_000]).unwrap();
    fs::write(photos.join("2024/b.txt"), b"bee").unwrap();
    fs::write(dir.join("notes.txt"), b"notes").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&photos, photos.join("loop")).unwrap();
        std::os::unix::fs::symlink(photos.join("a.jpg"), photos.join("link.jpg")).unwrap();
    }

    let d = f
        .download(
            json!({"t": "read", "paths": [s(&photos), s(&dir.join("notes.txt"))], "zip": true}),
            1,
        )
        .await
        .unwrap();
    let dir_name = dir.file_name().unwrap().to_string_lossy();
    assert_eq!(d.header["name"], format!("{dir_name}.zip"));
    let link_bytes = if cfg!(unix) { 300_000 } else { 0 };
    assert_eq!(d.header["total"], 300_000 + 3 + 5 + link_bytes);
    assert!(d.json.iter().any(|v| v["t"] == "progress"));
    let entries = zip_entries(&d.data);
    let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
    let mut expected = vec![
        "Photos/",
        "Photos/2024/",
        "Photos/2024/b.txt",
        "Photos/2024/empty/",
        "Photos/a.jpg",
    ];
    if cfg!(unix) {
        // The link to a file is zipped as that file; the link to a folder is left out.
        expected.push("Photos/link.jpg");
    }
    expected.push("notes.txt");
    assert_eq!(names, expected);
    let get = |n: &str| {
        entries
            .iter()
            .find(|e| e.0 == n)
            .unwrap()
            .1
            .clone()
            .unwrap()
    };
    assert_eq!(get("Photos/2024/b.txt"), b"bee");
    assert_eq!(get("Photos/a.jpg"), vec![1u8; 300_000]);

    // Unreadable files are left out and reported.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let locked = photos.join("2024/locked.txt");
        fs::write(&locked, b"secret").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let d = f
            .download(
                json!({"t": "read", "paths": [s(&photos.join("2024"))], "zip": true, "name": "x.zip"}),
                2,
            )
            .await
            .unwrap();
        assert_eq!(d.header["name"], "x.zip");
        let skipped = d.json.iter().find(|v| v["t"] == "skipped").unwrap();
        assert_eq!(skipped["paths"], json!(["2024/locked.txt"]));
        let names: Vec<String> = zip_entries(&d.data).into_iter().map(|e| e.0).collect();
        assert!(!names.iter().any(|n| n.contains("locked")));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mixed = f
        .download(
            json!({"t": "read", "paths": [s(&photos), s(&photos.join("a.jpg"))], "zip": true}),
            3,
        )
        .await
        .unwrap_err();
    assert_eq!(mixed["code"], "bad_request");
    fs::remove_dir_all(&dir).unwrap();
    setup.agent.stop().await;
}

#[tokio::test]
async fn protocol_mismatch_and_bad_messages() {
    let mut setup = setup().await;
    let mut f = Files::open(&mut setup, CHANNEL_A).await;
    f.hello().await;
    f.send(json!({"t": "list", "id": 3})).await;
    assert_eq!(f.reply(3).await["code"], "bad_request");
    f.send(json!({"t": "from_the_future", "id": 4})).await;
    // Ignored: the next request is answered normally.
    let st = f.request(json!({"t": "stat", "path": "/"}), 5).await;
    assert_eq!(st["t"], "stat");
    drop(f);

    let mut g = Files::open(&mut setup, CHANNEL_B).await;
    g.send(json!({"t": "hello", "proto": 0})).await;
    let e = g.expect(|v| v["t"] == "err").await;
    assert_eq!(e["code"], "proto_mismatch");
    loop {
        if let Msg::Close(code) = g.next().await {
            assert_eq!(code, Some(4005));
            break;
        }
    }
    setup.agent.stop().await;
}

#[tokio::test]
async fn shutdown_closes_file_channels() {
    let (setup, mut f) = setup_files().await;
    let Setup { agent, .. } = setup;
    let stop = tokio::spawn(agent.stop());
    loop {
        if let Msg::Close(code) = f.next().await {
            assert_eq!(code, Some(1001));
            break;
        }
    }
    stop.await.unwrap();
}
