//! Terminal channels end to end (docs/terminal.md): the agent runtime against a fake gateway,
//! with the test playing the viewer on the terminal socket and a real `/bin/sh` in a pseudo
//! terminal (Unix only).

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::*;

use rd_agent_core::runtime::Timings;
use rd_proto::bin::{self, BinMsg, Ping, TermAck, term_in_message};
use rd_proto::terminal::MAX_OUT_CHUNK;
use serde_json::{Value, json};

const CHANNEL_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const CHANNEL_B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const CHANNEL_C: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";

/// The viewer end of a terminal channel.
struct Term {
    ws: Ws,
    /// Acknowledge every TERM_OUT at once.
    auto_ack: bool,
    /// Highest TERM_OUT seq seen.
    seq: u32,
    /// The output shown (from `start`).
    shown: Vec<u8>,
    start: u64,
    /// TERM_OUT frames whose offset jumped ahead of what was shown.
    jumps: u32,
    /// JSON that arrived while waiting for output.
    pending: Vec<Value>,
}

impl Term {
    async fn open(setup: &mut Setup, channel: &str) -> Term {
        send_json(
            &mut setup.control.ws,
            json!({"t": "terminal_request", "channel": channel, "user": "me@test", "client_ip": "127.0.0.1"}),
        )
        .await;
        let conn = setup.gw.session().await;
        assert_eq!(conn.path, format!("/ws/agent/terminal/{channel}"));
        assert_eq!(conn.auth.as_deref(), Some(&*format!("Bearer {TOKEN}")));
        Term {
            ws: conn.ws,
            auto_ack: true,
            seq: 0,
            shown: Vec::new(),
            start: 0,
            jumps: 0,
            pending: Vec::new(),
        }
    }

    /// Offset after the last byte shown.
    fn end(&self) -> u64 {
        self.start + self.shown.len() as u64
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.shown).into_owned()
    }

    /// `hello` for a new shell (`attach` None) or an attach; returns the agent's answer.
    async fn hello(&mut self, attach: Option<(&str, u64)>) -> Value {
        let mut hello = json!({"t": "hello", "proto": 1, "cols": 80, "rows": 24});
        if let Some((id, offset)) = attach {
            hello["attach"] = id.into();
            hello["offset"] = offset.into();
        }
        send_json(&mut self.ws, hello).await;
        let v = self.expect(|v| v["t"] == "hello" || v["t"] == "err").await;
        if v["t"] == "hello" {
            let offset = v["offset"].as_u64().unwrap();
            if v["reset"] == true {
                self.shown.clear();
            } else {
                assert_eq!(
                    offset,
                    self.end(),
                    "an attach without reset continues where we are"
                );
            }
            self.start = offset;
        }
        v
    }

    async fn input(&mut self, bytes: &[u8]) {
        send_bin(&mut self.ws, term_in_message(bytes)).await;
    }

    async fn ack(&mut self) {
        use futures_util::SinkExt as _;
        // Best effort: the agent may be closing the channel right now.
        let ack = TermAck { seq: self.seq }.to_message();
        let _ = self
            .ws
            .send(tokio_tungstenite::tungstenite::Message::Binary(ack.into()))
            .await;
    }

    /// Next message; TERM_OUT is shown (and acknowledged) and then returned, PONG returned as
    /// `{"t":"pong"}`.
    async fn next(&mut self) -> Msg {
        let msg = recv(&mut self.ws).await;
        if let Msg::Bin(b) = &msg {
            match bin::parse(b) {
                Ok(BinMsg::TermOut(h, data)) => {
                    assert_eq!(h.seq, self.seq + 1, "TERM_OUT seq counts up");
                    assert!(data.len() <= MAX_OUT_CHUNK);
                    assert!(!data.is_empty());
                    self.seq = h.seq;
                    if h.offset != self.end() {
                        assert!(h.offset > self.end(), "output never goes back");
                        self.jumps += 1;
                        self.shown.clear();
                        self.start = h.offset;
                    }
                    self.shown.extend_from_slice(data);
                    if self.auto_ack {
                        self.ack().await;
                    }
                }
                Ok(BinMsg::Pong(p)) => return Msg::Json(json!({"t": "pong", "id": p.id})),
                _ => {}
            }
        }
        msg
    }

    /// The next JSON message matching `pred` (others are kept, `state` dropped).
    async fn expect(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.pending.iter().position(&pred) {
            return self.pending.remove(i);
        }
        let wait = async {
            loop {
                match self.next().await {
                    Msg::Json(v) if pred(&v) => return v,
                    Msg::Json(v) if v["t"] == "state" => {}
                    Msg::Json(v) => self.pending.push(v),
                    Msg::Bin(_) => {}
                    Msg::Close(code) => panic!("terminal channel closed ({code:?})"),
                }
            }
        };
        tokio::time::timeout(EXPECT_WAIT, wait)
            .await
            .expect("timed out waiting for a terminal message")
    }

    /// Reads until the output shown contains `needle`.
    async fn until_output(&mut self, needle: &str) {
        let wait = async {
            while !self.text().contains(needle) {
                match self.next().await {
                    Msg::Json(v) if v["t"] == "state" => {}
                    Msg::Json(v) => self.pending.push(v),
                    Msg::Bin(_) => {}
                    Msg::Close(code) => panic!(
                        "terminal channel closed ({code:?}) waiting for {needle:?}; shown: {:?}",
                        self.text()
                    ),
                }
            }
        };
        if tokio::time::timeout(EXPECT_WAIT, wait).await.is_err() {
            panic!("timed out waiting for {needle:?}; shown: {:?}", self.text());
        }
    }

    /// Waits for the agent's close (output on the way is shown).
    async fn expect_close(&mut self) -> Option<u16> {
        loop {
            match self.next().await {
                Msg::Close(code) => return code,
                Msg::Json(v) => self.pending.push(v),
                Msg::Bin(_) => {}
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_survives_a_reconnect_and_exits() {
    let mut setup = setup().await;
    let caps = &setup.control.register["caps"];
    assert_eq!(*caps, json!(["files", "text", "terminal"]));

    let mut a = Term::open(&mut setup, CHANNEL_A).await;
    let hello = a.hello(None).await;
    assert_eq!(hello["proto"], 1);
    assert_eq!(hello["os"], "fakeos");
    assert_eq!(hello["shell"], "sh");
    assert_eq!(hello["offset"], 0);
    assert_eq!(hello["reset"], true);
    assert_eq!(hello["state"]["in_session"], false);
    let id = hello["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);

    // The arithmetic is only in the output, not in the echo of what was typed.
    a.input(b"echo $((6*7)) \"$TERM $COLORTERM $TERM_PROGRAM\" ${RD_AGENT_TOKEN-none}\r")
        .await;
    a.until_output("42 xterm-256color truecolor rd none").await;
    send_json(&mut a.ws, json!({"t": "resize", "cols": 100, "rows": 30})).await;
    a.input(b"stty size\r").await;
    a.until_output("30 100").await;

    // Something that prints after the viewer went away.
    a.input(b"sleep 0.3; echo later-$((5*5))\r").await;
    a.until_output("later-$((5*5))").await;
    let shown = a.end();
    a.ws.close(None).await.unwrap();
    drop(a);
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Reattached: continues at our offset, with what the shell printed meanwhile.
    let mut b = Term::open(&mut setup, CHANNEL_B).await;
    b.start = shown;
    let hello = b.hello(Some((&id, shown))).await;
    assert_eq!(hello["id"], id);
    assert_eq!(hello["offset"], shown);
    assert_eq!(hello["reset"], false);
    b.until_output("later-25").await;

    // A third viewer takes the shell over; it gets everything from the start.
    let mut c = Term::open(&mut setup, CHANNEL_C).await;
    let hello = c.hello(Some((&id, 0))).await;
    assert_eq!(
        (hello["offset"].as_u64(), hello["reset"].as_bool()),
        (Some(0), Some(false))
    );
    let detached = b.expect(|v| v["t"] == "detached").await;
    assert_eq!(detached["reason"], "taken_over");
    assert_eq!(b.expect_close().await, Some(4001));
    c.until_output("later-25").await;
    assert!(c.text().contains("42 xterm-256color"));

    // PING is answered at once.
    send_bin(
        &mut c.ws,
        Ping {
            id: 7,
            t_origin_us: 1,
        }
        .to_message(),
    )
    .await;
    let pong = c.expect(|v| v["t"] == "pong").await;
    assert_eq!(pong["id"], 7);

    // `exit`: the output, then `exit`, then the close.
    c.input(b"echo bye-$((1+1)); exit 3\r").await;
    let exit = c.expect(|v| v["t"] == "exit").await;
    assert!(c.text().contains("bye-2"), "{}", c.text());
    assert_eq!(exit, json!({"t": "exit", "code": 3}));
    assert_eq!(c.expect_close().await, Some(1000));

    // The shell is gone.
    let mut d = Term::open(&mut setup, CHANNEL_A).await;
    let err = d.hello(Some((&id, 0))).await;
    assert_eq!(err["code"], "not_found");
    assert_eq!(d.expect_close().await, Some(1000));
    setup.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_hangs_the_shell_up() {
    let mut setup = setup().await;
    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    let hello = t.hello(None).await;
    let id = hello["id"].as_str().unwrap().to_owned();
    t.input(b"echo ready-$((2+2))\r").await;
    t.until_output("ready-4").await;
    send_json(&mut t.ws, json!({"t": "close"})).await;
    let exit = t.expect(|v| v["t"] == "exit").await;
    // sh ends on SIGHUP (some re-raise it, some exit with 129).
    assert!(
        exit["signal"] == 1 || exit["code"] == 129,
        "unexpected exit {exit}"
    );
    assert_eq!(t.expect_close().await, Some(1000));
    let mut again = Term::open(&mut setup, CHANNEL_B).await;
    assert_eq!(again.hello(Some((&id, 0))).await["code"], "not_found");
    setup.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_through_the_window() {
    let mut setup = setup().await;
    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    t.hello(None).await;
    t.input(b"echo ready-$((3+3))\r").await;
    t.until_output("ready-6").await;

    // A viewer that acknowledges nothing gets one window (64 KiB to start with) and no more,
    // however much the shell prints.
    t.auto_ack = false;
    let before = t.end();
    t.input(b"head -c 3000000 /dev/zero | tr '\\0' x; echo; echo done-$((4+4))\r")
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(msg) = tokio::time::timeout_at(deadline, t.next()).await {
        if let Msg::Close(code) = msg {
            panic!("closed ({code:?})");
        }
    }
    let unacked = t.end() - before;
    assert!(unacked <= 64 << 10, "{unacked} bytes without an ack");

    // Acknowledged, the rest flows; the shell printed more than the history keeps meanwhile,
    // so the output jumps ahead to the oldest byte the agent still has.
    t.auto_ack = true;
    t.ack().await;
    t.until_output("done-8").await;
    assert_eq!(t.jumps, 1);
    assert!(t.end() > 3_000_000);

    t.input(b"exit\r").await;
    assert_eq!(t.expect(|v| v["t"] == "exit").await["code"], 0);
    setup.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_paste_arrives_in_order() {
    let mut setup = setup().await;
    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    t.hello(None).await;
    // Nothing reads the terminal for a moment: the paste fills the terminal, then the
    // shell's input queue, then the channel stops reading its socket until there is room.
    t.input(b"stty -echo; echo go-$((1+1)); sleep 1; cksum\r")
        .await;
    t.until_output("go-2").await;
    // 2000 numbered lines in 40 frames, sent back to back: more than the input queue holds.
    let lines: Vec<u8> = (0..2000)
        .flat_map(|i| format!("{i:05} {}\n", "x".repeat(90)).into_bytes())
        .collect();
    for frame in lines.chunks(lines.len() / 40) {
        t.input(frame).await;
    }
    t.input(b"\x04").await;
    let expected = std::process::Command::new("cksum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin.take().unwrap().write_all(&lines)?;
            c.wait_with_output()
        })
        .unwrap();
    let expected = String::from_utf8(expected.stdout).unwrap();
    t.until_output(expected.trim()).await;
    t.input(b"exit\r").await;
    assert_eq!(t.expect(|v| v["t"] == "exit").await["code"], 0);
    setup.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_errors() {
    let timings = Timings {
        hello_timeout: Duration::from_millis(300),
        ..timings()
    };
    let mut setup = setup_with(FakePlatform::new(100), timings).await;

    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    send_json(
        &mut t.ws,
        json!({"t": "hello", "proto": 0, "cols": 80, "rows": 24}),
    )
    .await;
    assert_eq!(
        t.expect(|v| v["t"] == "err").await["code"],
        "proto_mismatch"
    );
    assert_eq!(t.expect_close().await, Some(4005));

    let mut t = Term::open(&mut setup, CHANNEL_B).await;
    assert_eq!(
        t.hello(Some(("0123456789abcdef0123456789abcdef", 0))).await["code"],
        "not_found"
    );
    assert_eq!(t.expect_close().await, Some(1000));

    let mut t = Term::open(&mut setup, CHANNEL_C).await;
    send_json(&mut t.ws, json!({"t": "resize", "cols": 80, "rows": 24})).await;
    assert_eq!(t.expect(|v| v["t"] == "err").await["code"], "bad_request");
    assert_eq!(t.expect_close().await, Some(1000));

    // No hello in time.
    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    assert_eq!(t.expect_close().await, Some(4005));
    setup.agent.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_agent_ends_the_shells() {
    let mut setup = setup().await;
    let mut t = Term::open(&mut setup, CHANNEL_A).await;
    t.hello(None).await;
    // A shell that ignores the hangup is killed.
    t.input(b"trap '' HUP; echo armed-$((9*9)) pid=$$.\r").await;
    t.until_output("armed-81").await;
    t.until_output(".").await;
    let text = t.text();
    let pid: i32 = text
        .split("armed-81 pid=")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {text:?}"));
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let Setup { agent, .. } = setup;
    agent.stop().await;
    assert_eq!(t.expect_close().await, Some(1001));
    // Hung up, then killed, before the runtime returned.
    let gone = unsafe { libc::kill(pid, 0) } != 0
        || std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim_start()
                    .starts_with('Z')
            });
    assert!(gone, "the shell {pid} still runs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_can_be_turned_off() {
    let mut setup = setup_configured(FakePlatform::new(100), timings(), |cfg| {
        cfg.terminal.enabled = false;
    })
    .await;
    assert_eq!(setup.control.register["caps"], json!(["files", "text"]));
    send_json(
        &mut setup.control.ws,
        json!({"t": "terminal_request", "channel": CHANNEL_A, "user": "me@test", "client_ip": "127.0.0.1"}),
    )
    .await;
    assert!(setup.gw.no_session_within(Duration::from_millis(500)).await);
    setup.agent.stop().await;
}
