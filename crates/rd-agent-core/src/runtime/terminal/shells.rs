//! The shells of the runtime (docs/terminal.md). They outlive channels and control reconnects:
//! a shell runs until it exits, is closed, is not attached for its keep time, or the runtime
//! stops.
//!
//! ```text
//! Shells           at most 8, attached or not
//!   └─ Shell       id, history, exit status, which channel shows it
//!        ├─ reader thread   drains the terminal into the history all the time, wakes the channel
//!        ├─ writer thread   writes TERM_IN bytes in order from a bounded queue
//!        ├─ waiter thread   the exit status, once the reader has everything
//!        └─ keep timer      ends the shell `keep_minutes` after its channel went away
//! ```

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use rd_proto::terminal::HISTORY_BYTES;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use super::history::History;
use crate::config::TerminalSettings;
use crate::platform::{Platform, PtyChild, PtyExit, PtyProcess, PtyRequest, PtySize};

/// Shells per agent, attached or not.
pub(crate) const MAX_SHELLS: usize = 8;
/// TERM_IN frames waiting for the writer thread (each at most 64 KiB).
const INPUT_QUEUE: usize = 16;
/// Bytes the reader takes from the terminal at a time.
const READ_BLOCK: usize = 16 << 10;
/// A shell still running this long after its hangup is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(3);
/// A shell still running this long after its output ended is hung up.
const OUTPUT_LOST_GRACE: Duration = Duration::from_secs(1);
/// How long runtime shutdown waits for hung-up shells before it kills them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Variables every shell gets, on top of the platform's own (docs/terminal.md, Settings).
pub(crate) fn shell_env(version: &str) -> Vec<(String, String)> {
    vec![
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "rd".into()),
        ("TERM_PROGRAM_VERSION".into(), version.into()),
    ]
}

/// Variables of the agent the shell must not see.
pub(crate) const ENV_REMOVE: &[&str] = &["RD_AGENT_TOKEN"];

/// Why no shell was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartError {
    /// [`MAX_SHELLS`] already run.
    Busy,
    /// The platform could not start it.
    Spawn(String),
}

pub(crate) struct Shells {
    platform: Arc<dyn Platform>,
    settings: TerminalSettings,
    version: String,
    keep: Duration,
    handle: tokio::runtime::Handle,
    inner: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    shells: HashMap<String, Arc<Shell>>,
    /// Slots taken by spawns in progress.
    starting: usize,
    /// The runtime stopped: no new shells.
    closed: bool,
}

impl Shells {
    pub(crate) fn new(
        platform: Arc<dyn Platform>,
        settings: TerminalSettings,
        version: String,
        handle: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let keep = settings.keep();
        Shells::with_keep(platform, settings, version, handle, keep)
    }

    /// Like [`Shells::new`] with any keep time (the tests use milliseconds).
    pub(crate) fn with_keep(
        platform: Arc<dyn Platform>,
        settings: TerminalSettings,
        version: String,
        handle: tokio::runtime::Handle,
        keep: Duration,
    ) -> Arc<Self> {
        Arc::new(Shells {
            platform,
            settings,
            version,
            keep,
            handle,
            inner: Mutex::default(),
        })
    }

    /// Starts a new shell of `size` for `user`.
    pub(crate) async fn start(
        self: &Arc<Self>,
        user: &str,
        size: PtySize,
    ) -> Result<Arc<Shell>, StartError> {
        {
            let mut reg = self.inner.lock();
            if reg.closed || reg.shells.len() + reg.starting >= MAX_SHELLS {
                return Err(StartError::Busy);
            }
            reg.starting += 1;
        }
        let req = PtyRequest {
            size,
            shell: self.settings.shell().map(str::to_owned),
            env: shell_env(&self.version),
            env_remove: ENV_REMOVE.iter().map(|&s| s.to_owned()).collect(),
        };
        let platform = self.platform.clone();
        let spawned = tokio::task::spawn_blocking(move || platform.spawn_pty(&req)).await;
        let result = match spawned {
            Ok(Ok(pty)) => Ok(self.adopt(pty, user)),
            Ok(Err(e)) => Err(StartError::Spawn(e.to_string())),
            Err(e) => Err(StartError::Spawn(format!("starting the shell failed: {e}"))),
        };
        let mut reg = self.inner.lock();
        reg.starting -= 1;
        match result {
            Ok(shell) if reg.closed => {
                drop(reg);
                shell.hang_up();
                Err(StartError::Busy)
            }
            Ok(shell) => {
                reg.shells.insert(shell.id.clone(), shell.clone());
                drop(reg);
                info!(
                    shell = shell.short(),
                    user,
                    name = shell.name,
                    "terminal: shell started"
                );
                Ok(shell)
            }
            Err(e) => {
                drop(reg);
                if let StartError::Spawn(msg) = &e {
                    warn!(user, "terminal: the shell could not be started: {msg}");
                }
                Err(e)
            }
        }
    }

    /// Wraps a started process: its threads begin at once.
    fn adopt(self: &Arc<Self>, pty: PtyProcess, user: &str) -> Arc<Shell> {
        let PtyProcess {
            name,
            reader,
            writer,
            child,
        } = pty;
        let (input, input_rx) = mpsc::channel(INPUT_QUEUE);
        let shell = Arc::new(Shell {
            id: format!("{:032x}", rand::random::<u128>()),
            name,
            user: user.to_owned(),
            child,
            input,
            state: Mutex::new(State {
                history: History::new(HISTORY_BYTES),
                exit: None,
                attached: None,
                generation: 0,
                hanging_up: false,
                keep_timer: None,
            }),
            shells: Arc::downgrade(self),
            handle: self.handle.clone(),
        });
        let spawn = |what: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(format!("rd-term-{what}"))
                .spawn(f)
        };
        let reader_shell = shell.clone();
        let reader = spawn("read", Box::new(move || reader_shell.read_loop(reader)));
        let _ = spawn("write", Box::new(move || write_loop(writer, input_rx)));
        let waiter_shell = shell.clone();
        let waited = spawn(
            "wait",
            Box::new(move || {
                let exit = waiter_shell.child.wait();
                if let Ok(reader) = reader {
                    let _ = reader.join();
                }
                waiter_shell.exited(exit);
            }),
        );
        if let Err(e) = waited {
            // Without a waiter nobody would notice the end: end it now.
            warn!("terminal: no thread for the shell ({e}); ending it");
            shell.child.kill();
            shell.exited(PtyExit::default());
        }
        shell
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Shell>> {
        self.inner.lock().shells.get(id).cloned()
    }

    /// Forgets a shell whose exit was delivered.
    pub(crate) fn remove(&self, shell: &Shell) {
        let mut reg = self.inner.lock();
        if reg
            .shells
            .get(&shell.id)
            .is_some_and(|s| std::ptr::eq(&**s, shell))
        {
            reg.shells.remove(&shell.id);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().shells.len()
    }

    /// The keep time of `id` (detached at `generation`) ran out.
    fn expire(&self, id: &str, generation: u64) {
        let shell = {
            let mut reg = self.inner.lock();
            let Some(shell) = reg.shells.get(id).cloned() else {
                return;
            };
            let st = shell.state.lock();
            if st.attached.is_some() || st.generation != generation {
                return;
            }
            drop(st);
            reg.shells.remove(id);
            shell
        };
        info!(
            shell = shell.short(),
            "terminal: shell ended after its keep time"
        );
        shell.hang_up();
    }

    /// The runtime stops: hangs up every shell and kills those that are still running after
    /// a moment.
    pub(crate) async fn shutdown(&self) {
        let shells: Vec<Arc<Shell>> = {
            let mut reg = self.inner.lock();
            reg.closed = true;
            reg.shells.drain().map(|(_, s)| s).collect()
        };
        if shells.is_empty() {
            return;
        }
        info!(shells = shells.len(), "terminal: ending the shells");
        for shell in &shells {
            let child = shell.child.clone();
            let _ = tokio::task::spawn_blocking(move || child.hangup()).await;
        }
        if wait_exited(&shells, SHUTDOWN_GRACE).await {
            return;
        }
        for shell in shells.iter().filter(|s| !s.has_exited()) {
            warn!(
                shell = shell.short(),
                "terminal: shell still running after its hangup; killing it"
            );
            let child = shell.child.clone();
            let _ = tokio::task::spawn_blocking(move || child.kill()).await;
        }
        if !wait_exited(&shells, SHUTDOWN_GRACE).await {
            warn!("terminal: a shell did not end");
        }
    }
}

/// Waits (bounded) until every shell exited; false on time out.
async fn wait_exited(shells: &[Arc<Shell>], limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if shells.iter().all(|s| s.has_exited()) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One shell.
pub(crate) struct Shell {
    /// 32 hex digits, 128 random bits.
    pub(crate) id: String,
    /// For the title ("zsh").
    pub(crate) name: String,
    /// Who started it (for logs).
    user: String,
    child: Arc<dyn PtyChild>,
    input: mpsc::Sender<Bytes>,
    state: Mutex<State>,
    shells: Weak<Shells>,
    handle: tokio::runtime::Handle,
}

struct State {
    history: History,
    /// Set once the shell exited and all its output is in `history`.
    exit: Option<PtyExit>,
    attached: Option<Attached>,
    /// Counts attaches and detaches: a keep timer ends the shell only if nothing changed.
    generation: u64,
    hanging_up: bool,
    keep_timer: Option<AbortHandle>,
}

struct Attached {
    generation: u64,
    wake: Arc<Notify>,
    kick: Option<oneshot::Sender<()>>,
}

/// A channel's hold on a shell, from [`Shell::attach`].
pub(crate) struct Attachment {
    pub(crate) generation: u64,
    /// Notified when output arrives or the shell exits.
    pub(crate) wake: Arc<Notify>,
    /// Fires when another channel takes the shell over.
    pub(crate) kicked: oneshot::Receiver<()>,
}

/// What a channel at some cursor can send.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    /// Where its output starts (past the cursor if the history moved on).
    pub(crate) offset: u64,
    pub(crate) len: usize,
    /// The shell's exit, once there is no more output after `offset`.
    pub(crate) exit: Option<PtyExit>,
}

impl Shell {
    /// The first 8 digits of the id, for logs.
    pub(crate) fn short(&self) -> &str {
        &self.id[..8]
    }

    /// Attaches a channel; the one that showed the shell so far is kicked.
    pub(crate) fn attach(&self, user: &str) -> Attachment {
        let (kick, kicked) = oneshot::channel();
        let wake = Arc::new(Notify::new());
        let mut st = self.state.lock();
        st.generation += 1;
        if let Some(timer) = st.keep_timer.take() {
            timer.abort();
        }
        let generation = st.generation;
        let previous = st.attached.replace(Attached {
            generation,
            wake: wake.clone(),
            kick: Some(kick),
        });
        drop(st);
        if let Some(mut previous) = previous {
            info!(
                shell = self.short(),
                user, "terminal: shell taken over by another channel"
            );
            if let Some(kick) = previous.kick.take() {
                let _ = kick.send(());
            }
        } else {
            info!(shell = self.short(), user, "terminal: shell attached");
        }
        Attachment {
            generation,
            wake,
            kicked,
        }
    }

    /// The channel of `generation` went away (no-op if another one took over): the keep time
    /// starts.
    pub(crate) fn detach(self: &Arc<Self>, generation: u64, user: &str) {
        let mut st = self.state.lock();
        if st
            .attached
            .as_ref()
            .is_none_or(|a| a.generation != generation)
        {
            return;
        }
        st.attached = None;
        st.generation += 1;
        let generation = st.generation;
        let Some(shells) = self.shells.upgrade() else {
            return;
        };
        let keep = shells.keep;
        let id = self.id.clone();
        let weak = self.shells.clone();
        let timer = self.handle.spawn(async move {
            tokio::time::sleep(keep).await;
            if let Some(shells) = weak.upgrade() {
                shells.expire(&id, generation);
            }
        });
        st.keep_timer = Some(timer.abort_handle());
        let exited = st.exit.is_some();
        drop(st);
        info!(
            shell = self.short(),
            user,
            exited,
            keep_s = keep.as_secs(),
            "terminal: shell detached"
        );
    }

    /// Where an attach from the viewer's `offset` starts and whether it is a reset.
    pub(crate) fn resume(&self, offset: u64) -> (u64, bool) {
        self.state.lock().history.resume(offset)
    }

    /// What a channel at `cursor` may send next, at most `max` bytes of it.
    pub(crate) fn pending(&self, cursor: u64, max: usize) -> Pending {
        let st = self.state.lock();
        let offset = st.history.clamp(cursor);
        let len = ((st.history.end() - offset) as usize).min(max);
        Pending {
            offset,
            len,
            exit: if len == 0 { st.exit } else { None },
        }
    }

    /// `len` bytes from `offset` (as [`Shell::pending`] returned them; fewer if the history
    /// moved on meanwhile): (their offset, the bytes).
    pub(crate) fn read(&self, offset: u64, len: usize) -> (u64, Vec<u8>) {
        self.state.lock().history.read(offset, len)
    }

    /// Where the shell's input goes, in order.
    pub(crate) fn input(&self) -> mpsc::Sender<Bytes> {
        self.input.clone()
    }

    pub(crate) fn resize(&self, size: PtySize) {
        if let Err(e) = self.child.resize(size) {
            debug!(shell = self.short(), "terminal: resize failed: {e}");
        }
    }

    pub(crate) fn has_exited(&self) -> bool {
        self.state.lock().exit.is_some()
    }

    /// Ends the shell: hangup now, kill after [`HANGUP_GRACE`]. Its `exit` follows.
    pub(crate) fn hang_up(self: &Arc<Self>) {
        {
            let mut st = self.state.lock();
            if st.exit.is_some() || st.hanging_up {
                return;
            }
            st.hanging_up = true;
        }
        let shell = self.clone();
        self.handle.spawn(async move {
            let child = shell.child.clone();
            let _ = tokio::task::spawn_blocking(move || child.hangup()).await;
            tokio::time::sleep(HANGUP_GRACE).await;
            if !shell.has_exited() {
                warn!(
                    shell = shell.short(),
                    "terminal: shell still running after its hangup; killing it"
                );
                let child = shell.child.clone();
                let _ = tokio::task::spawn_blocking(move || child.kill()).await;
            }
        });
    }

    fn wake(st: &State) {
        if let Some(a) = &st.attached {
            a.wake.notify_one();
        }
    }

    /// The reader thread: everything the shell prints goes into the history, whether a channel
    /// is attached or not, so the shell never waits for a viewer.
    fn read_loop(self: Arc<Self>, mut reader: Box<dyn std::io::Read + Send>) {
        let mut buf = vec![0u8; READ_BLOCK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut st = self.state.lock();
                    st.history.push(&buf[..n]);
                    Shell::wake(&st);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    debug!(
                        shell = self.short(),
                        "terminal: reading the shell's output ended: {e}"
                    );
                    break;
                }
            }
        }
        // Normally the shell has exited and the waiter reports it right away. If it still
        // runs, nothing it prints can be shown any more: end it (the waiter then reports it).
        let shell = self.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(OUTPUT_LOST_GRACE).await;
            if !shell.has_exited() {
                warn!(
                    shell = shell.short(),
                    "terminal: the shell's output is gone; ending it"
                );
                shell.hang_up();
            }
        });
    }

    /// The waiter thread, once the shell exited and its output is all read.
    fn exited(&self, exit: PtyExit) {
        let mut st = self.state.lock();
        st.exit = Some(exit);
        let attached = st.attached.is_some();
        let bytes = st.history.end();
        Shell::wake(&st);
        drop(st);
        info!(
            shell = self.short(),
            user = self.user,
            code = exit.code,
            signal = exit.signal,
            bytes,
            attached,
            "terminal: shell exited"
        );
    }
}

/// The writer thread: TERM_IN bytes to the shell, in order. The bounded queue in front of it
/// holds back the channel (and so the viewer) while the shell does not read its input.
fn write_loop(mut writer: Box<dyn std::io::Write + Send>, mut input: mpsc::Receiver<Bytes>) {
    while let Some(bytes) = input.blocking_recv() {
        if let Err(e) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            debug!("terminal: writing to the shell ended: {e}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::Instant;

    use crate::platform::{
        DisplayDesc, Guard, InputInjector, LockState, Perms, PtyRequest, VideoPipeline,
    };

    /// A pseudo terminal in memory: input is echoed as output; "exit\r" ends it with code 0,
    /// a hangup with signal 1 (unless it ignores hangups), a kill with signal 9.
    struct FakePty {
        out: Mutex<Option<crossbeam_channel::Sender<Vec<u8>>>>,
        exit: Mutex<Option<PtyExit>>,
        exited: std::sync::Condvar,
        exit_lock: std::sync::Mutex<()>,
        sizes: Mutex<Vec<PtySize>>,
        ignore_hangup: bool,
        hangups: Mutex<u32>,
    }

    impl FakePty {
        fn print(&self, data: &[u8]) {
            if let Some(out) = &*self.out.lock() {
                let _ = out.send(data.to_vec());
            }
        }

        fn finish(&self, exit: PtyExit) {
            let mut slot = self.exit.lock();
            if slot.is_none() {
                *slot = Some(exit);
                self.out.lock().take();
                let _guard = self.exit_lock.lock().unwrap();
                self.exited.notify_all();
            }
        }
    }

    struct FakeReader(crossbeam_channel::Receiver<Vec<u8>>, Vec<u8>);

    impl io::Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(data) => self.1 = data,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.1.len());
            buf[..n].copy_from_slice(&self.1[..n]);
            self.1.drain(..n);
            Ok(n)
        }
    }

    struct FakeWriter(Arc<FakePty>);

    impl io::Write for FakeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.0.exit.lock().is_some() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.0.print(buf);
            if buf == b"exit\r" {
                self.0.finish(PtyExit {
                    code: Some(0),
                    signal: None,
                });
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeChild(Arc<FakePty>);

    impl PtyChild for FakeChild {
        fn resize(&self, size: PtySize) -> io::Result<()> {
            self.0.sizes.lock().push(size);
            Ok(())
        }
        fn hangup(&self) {
            *self.0.hangups.lock() += 1;
            if !self.0.ignore_hangup {
                self.0.finish(PtyExit {
                    code: None,
                    signal: Some(1),
                });
            }
        }
        fn kill(&self) {
            self.0.finish(PtyExit {
                code: None,
                signal: Some(9),
            });
        }
        fn wait(&self) -> PtyExit {
            let mut guard = self.0.exit_lock.lock().unwrap();
            loop {
                if let Some(exit) = *self.0.exit.lock() {
                    return exit;
                }
                guard = self.0.exited.wait(guard).unwrap();
            }
        }
    }

    #[derive(Default)]
    struct FakePlatform {
        ptys: Mutex<Vec<Arc<FakePty>>>,
        requests: Mutex<Vec<PtyRequest>>,
        fail: Mutex<Option<String>>,
        ignore_hangup: bool,
    }

    impl Platform for FakePlatform {
        fn os(&self) -> &'static str {
            "fakeos"
        }
        fn os_description(&self) -> String {
            String::new()
        }
        fn displays(&self) -> Vec<DisplayDesc> {
            Vec::new()
        }
        fn native_size(&self, _display: u32) -> Option<(u32, u32)> {
            None
        }
        fn permissions(&self) -> Perms {
            Perms::default()
        }
        fn lock_state(&self) -> LockState {
            LockState::Unlocked
        }
        fn keep_awake(&self, _reason: &str) -> Box<dyn Guard> {
            Box::new(())
        }
        fn declare_user_activity(&self) {}
        fn video_pipeline(&self) -> anyhow::Result<Box<dyn VideoPipeline>> {
            anyhow::bail!("no video")
        }
        fn input_injector(&self) -> anyhow::Result<Box<dyn InputInjector>> {
            anyhow::bail!("no input")
        }
        fn pty_supported(&self) -> bool {
            true
        }
        fn spawn_pty(&self, req: &PtyRequest) -> io::Result<PtyProcess> {
            self.requests.lock().push(req.clone());
            if let Some(msg) = self.fail.lock().clone() {
                return Err(io::Error::other(msg));
            }
            let (tx, rx) = crossbeam_channel::unbounded();
            let pty = Arc::new(FakePty {
                out: Mutex::new(Some(tx)),
                exit: Mutex::new(None),
                exited: std::sync::Condvar::new(),
                exit_lock: std::sync::Mutex::new(()),
                sizes: Mutex::new(vec![req.size]),
                ignore_hangup: self.ignore_hangup,
                hangups: Mutex::new(0),
            });
            self.ptys.lock().push(pty.clone());
            Ok(PtyProcess {
                name: "fakesh".into(),
                reader: Box::new(FakeReader(rx, Vec::new())),
                writer: Box::new(FakeWriter(pty.clone())),
                child: Arc::new(FakeChild(pty)),
            })
        }
    }

    const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

    fn shells_with(platform: Arc<FakePlatform>, keep: Duration) -> Arc<Shells> {
        let settings = TerminalSettings {
            shell: Some("fakesh -x".into()),
            ..TerminalSettings::default()
        };
        Shells::with_keep(
            platform,
            settings,
            "9.9.9".into(),
            tokio::runtime::Handle::current(),
            keep,
        )
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn type_in(shell: &Shell, bytes: &[u8]) {
        shell
            .input()
            .send(Bytes::copy_from_slice(bytes))
            .await
            .unwrap();
    }

    /// Output from `cursor` up to the end.
    fn output(shell: &Shell, cursor: u64) -> (u64, Vec<u8>) {
        let p = shell.pending(cursor, usize::MAX);
        shell.read(p.offset, p.len)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_attach_output_exit() {
        let platform = Arc::new(FakePlatform::default());
        let shells = shells_with(platform.clone(), Duration::from_secs(60));
        let shell = shells.start("me@test", SIZE).await.unwrap();
        assert_eq!(shell.id.len(), 32);
        assert!(shell.id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(shell.name, "fakesh");
        assert_eq!(shells.get(&shell.id).unwrap().id, shell.id);
        // The request: the configured shell, the environment changes.
        let req = platform.requests.lock()[0].clone();
        assert_eq!(req.shell.as_deref(), Some("fakesh -x"));
        assert!(
            req.env
                .contains(&("TERM_PROGRAM_VERSION".into(), "9.9.9".into()))
        );
        assert!(req.env.contains(&("COLORTERM".into(), "truecolor".into())));
        assert_eq!(req.env_remove, ["RD_AGENT_TOKEN"]);

        let att = shell.attach("me@test");
        type_in(&shell, b"ls\r").await;
        tokio::time::timeout(Duration::from_secs(5), att.wake.notified())
            .await
            .expect("woken by output");
        until("the echo", || shell.pending(0, 100).len == 3).await;
        assert_eq!(output(&shell, 0), (0, b"ls\r".to_vec()));
        assert_eq!(output(&shell, 1), (1, b"s\r".to_vec()));
        assert_eq!(
            shell.pending(3, 100),
            Pending {
                offset: 3,
                len: 0,
                exit: None
            }
        );
        shell.resize(PtySize {
            cols: 100,
            rows: 30,
        });
        assert_eq!(platform.ptys.lock()[0].sizes.lock().len(), 2);

        type_in(&shell, b"exit\r").await;
        until("the exit", || shell.has_exited()).await;
        // The exit shows only once the output before it was read.
        assert_eq!(shell.pending(3, 100).exit, None);
        assert_eq!(shell.pending(3, 100).len, 5);
        assert_eq!(
            shell.pending(8, 100).exit,
            Some(PtyExit {
                code: Some(0),
                signal: None
            })
        );
        shells.remove(&shell);
        assert!(shells.get(&shell.id).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_and_spawn_errors() {
        let platform = Arc::new(FakePlatform::default());
        let shells = shells_with(platform.clone(), Duration::from_secs(60));
        let mut started = Vec::new();
        for _ in 0..MAX_SHELLS {
            started.push(shells.start("me", SIZE).await.unwrap());
        }
        assert_eq!(shells.start("me", SIZE).await.err(), Some(StartError::Busy));
        // Exited but not yet delivered shells count too, until they are removed.
        started[0].hang_up();
        until("the hangup", || started[0].has_exited()).await;
        assert_eq!(shells.start("me", SIZE).await.err(), Some(StartError::Busy));
        shells.remove(&started[0]);
        *platform.fail.lock() = Some("no such shell".into());
        assert_eq!(
            shells.start("me", SIZE).await.err(),
            Some(StartError::Spawn("no such shell".into()))
        );
        assert_eq!(shells.len(), MAX_SHELLS - 1);
        shells.shutdown().await;
        assert_eq!(shells.len(), 0);
        assert!(started.iter().all(|s| s.has_exited()));
        // No new shells after shutdown.
        *platform.fail.lock() = None;
        assert_eq!(shells.start("me", SIZE).await.err(), Some(StartError::Busy));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn takeover_kicks_the_previous_channel() {
        let shells = shells_with(Arc::default(), Duration::from_millis(50));
        let shell = shells.start("me", SIZE).await.unwrap();
        let mut first = shell.attach("me");
        let second = shell.attach("you");
        tokio::time::timeout(Duration::from_secs(1), &mut first.kicked)
            .await
            .expect("kicked")
            .expect("kicked, not dropped");
        // The kicked channel's detach changes nothing: the shell stays attached.
        shell.detach(first.generation, "me");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(shells.get(&shell.id).is_some());
        assert!(!shell.has_exited());
        // The current channel's detach starts the keep time.
        shell.detach(second.generation, "you");
        until("the keep time", || shells.get(&shell.id).is_none()).await;
        until("the hangup", || shell.has_exited()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reattach_within_the_keep_time() {
        let shells = shells_with(Arc::default(), Duration::from_millis(100));
        let shell = shells.start("me", SIZE).await.unwrap();
        let att = shell.attach("me");
        type_in(&shell, b"abc").await;
        until("the echo", || shell.pending(0, 10).len == 3).await;
        shell.detach(att.generation, "me");
        type_in(&shell, b"def").await;
        until("output while detached", || shell.pending(0, 10).len == 6).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Reattached in time: the timer is off, the replay continues at the viewer's offset.
        let att = shell.attach("me");
        assert_eq!(shell.resume(3), (3, false));
        assert_eq!(output(&shell, 3), (3, b"def".to_vec()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(shells.get(&shell.id).is_some());
        // A second detach starts a new keep time.
        shell.detach(att.generation, "me");
        until("the keep time", || shells.get(&shell.id).is_none()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exited_while_detached_is_kept_for_the_replay() {
        let shells = shells_with(Arc::default(), Duration::from_millis(300));
        let shell = shells.start("me", SIZE).await.unwrap();
        let att = shell.attach("me");
        shell.detach(att.generation, "me");
        type_in(&shell, b"exit\r").await;
        until("the exit", || shell.has_exited()).await;
        // Still there: a reattach replays the output, then gets the exit.
        let again = shells.get(&shell.id).expect("kept");
        let _att = again.attach("me");
        assert_eq!(again.resume(0), (0, false));
        assert_eq!(output(&again, 0), (0, b"exit\r".to_vec()));
        assert_eq!(again.pending(5, 10).exit.unwrap().code, Some(0));
        // Detached again, it is dropped when the keep time ran out.
        again.detach(_att.generation, "me");
        until("the keep time", || shells.get(&shell.id).is_none()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_overtakes_a_slow_channel() {
        let shells = shells_with(Arc::default(), Duration::from_secs(60));
        let shell = shells.start("me", SIZE).await.unwrap();
        let _att = shell.attach("me");
        let block = vec![b'x'; 64 << 10];
        for _ in 0..(HISTORY_BYTES / block.len() + 2) {
            type_in(&shell, &block).await;
        }
        let total = (HISTORY_BYTES + 2 * block.len()) as u64;
        until("all output", || shell.pending(total, 1).offset == total).await;
        // A channel still at 0 continues with the oldest byte kept.
        let p = shell.pending(0, 1000);
        assert_eq!(p.offset, total - HISTORY_BYTES as u64);
        assert_eq!(p.len, 1000);
        assert_eq!(shell.resume(10), (total - HISTORY_BYTES as u64, true));
        shell.hang_up();
        until("the hangup", || shell.has_exited()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn hangup_escalates_to_kill() {
        let platform = Arc::new(FakePlatform {
            ignore_hangup: true,
            ..FakePlatform::default()
        });
        let shells = shells_with(platform.clone(), Duration::from_secs(60));
        let shell = shells.start("me", SIZE).await.unwrap();
        shell.hang_up();
        shell.hang_up();
        // Virtual time: the sleeps below let the 3 s grace pass at once.
        let started = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!shell.has_exited());
        until("the kill", || shell.has_exited()).await;
        assert!(started.elapsed() >= HANGUP_GRACE);
        assert_eq!(*platform.ptys.lock()[0].hangups.lock(), 1);
        assert_eq!(shell.pending(0, 10).exit.unwrap().signal, Some(9));
    }
}
