//! Secure desktop: the Windows lock screen and UAC prompt, shown in the viewer.
//!
//! The agent runs in the user's session and cannot capture or inject the Winlogon desktop; it
//! is a different desktop owned by SYSTEM. `rd-agent-svc.exe` (LocalSystem, installed once with
//! administrator rights) launches `rd-agent-sec.exe` into the console session as SYSTEM while
//! the host is locked. The helper opens the input desktop (`Winlogon`), captures it with GDI,
//! and injects input with `SendInput`; it talks to this module over a named pipe whose ACL
//! allows SYSTEM only.
//!
//! The viewer keeps its normal session: [`SecurePipeline`] feeds the helper's frames through
//! the same encoder and [`SecureInputInjector`] forwards input to the helper. Nothing is
//! stored: the user types the Windows password into the picture, at the real lock screen.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::clock::now_us;
use rd_agent_core::nv12;
use rd_agent_core::platform::{InputEvent, InputInjector, SecureState, VideoConfig, VideoPipeline};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    PeekNamedPipe,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::HSTRING;

pub mod helper;
pub mod service;

/// The service's display name, also its SCM key: `sc query rd-agent-secure`.
pub const SERVICE_NAME: &str = "rd-agent-secure";
/// The service's control pipe. Its ACL allows interactive users; the service checks the
/// caller's session and SID before it starts anything.
pub const CONTROL_PIPE: &str = r"\\.\pipe\rd-secure-control";
/// Data pipes created by the agent: `\\.\pipe\rd-secure-<128-bit token>`.
pub const DATA_PIPE_PREFIX: &str = r"\\.\pipe\rd-secure-";

/// Messages on the data pipe. A message is `[kind u8][len u32 LE][payload]`.
pub const MSG_CONFIG: u8 = 1;
pub const MSG_INPUT: u8 = 2;
pub const MSG_SAS: u8 = 3;
pub const MSG_STOP: u8 = 4;
pub const MSG_FRAME: u8 = 10;
pub const MSG_ERROR: u8 = 11;
pub const MSG_READY: u8 = 12;

/// The largest frame we accept: 4K BGRA is 33 MiB.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// What the agent asks the service to do on the control pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SecureRequest {
    /// Launch `rd-agent-sec.exe <pipe>` in the console session, as SYSTEM.
    Start { pipe: String },
    /// Terminate the helper and close the data pipe.
    Stop,
}

/// The service's answer; one line before the data pipe starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SecureReply {
    Ok,
    Error { msg: String },
}

// ---------------------------------------------------------------------------
// named pipes

/// A synchronous named-pipe handle. Reads and writes may happen on different threads.
pub struct Pipe(HANDLE);

// SAFETY: a pipe handle is a kernel object and may be used from any thread; writes are
// serialized by `SecureDesktop`'s mutex, reads by the pipeline's reader thread.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    pub fn from_handle(handle: HANDLE) -> Self {
        Pipe(handle)
    }

    pub fn handle(&self) -> HANDLE {
        self.0
    }

    pub fn open(name: &str) -> anyhow::Result<Self> {
        // SAFETY: a plain open of an existing pipe; the handle is owned by the returned Pipe.
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(name),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .with_context(|| format!("opening {name}"))?;
        Ok(Pipe(handle))
    }

    pub fn read(&self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let mut n = 0u32;
        // SAFETY: `buf` is writable for its whole length; the handle is open.
        unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None) }.context("reading a pipe")?;
        Ok(n as usize)
    }

    pub fn read_exact(&self, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.read(&mut buf[off..])?;
            if n == 0 {
                bail!("the pipe closed");
            }
            off += n;
        }
        Ok(())
    }

    pub fn write_all(&self, buf: &[u8]) -> anyhow::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let mut n = 0u32;
            // SAFETY: `buf[off..]` is readable; the handle is open.
            unsafe { WriteFile(self.0, Some(&buf[off..]), Some(&mut n), None) }
                .context("writing a pipe")?;
            if n == 0 {
                bail!("the pipe closed");
            }
            off += n as usize;
        }
        Ok(())
    }

    /// Bytes waiting in the pipe (0 when empty).
    pub fn peek(&self) -> anyhow::Result<u32> {
        let mut available = 0u32;
        // SAFETY: a query of the pipe's buffer; no data is consumed.
        unsafe { PeekNamedPipe(self.0, None, 0, None, Some(&mut available), None) }
            .context("peeking a pipe")?;
        Ok(available)
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: this Pipe owns the handle.
        unsafe { CloseHandle(self.0) }.ok();
    }
}

/// A `[kind][len]` message on the data pipe.
pub fn write_message(pipe: &Pipe, kind: u8, payload: &[u8]) -> anyhow::Result<()> {
    ensure!(
        payload.len() <= MAX_MESSAGE,
        "secure desktop message too large ({} bytes)",
        payload.len()
    );
    let mut msg = Vec::with_capacity(5 + payload.len());
    msg.push(kind);
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(payload);
    pipe.write_all(&msg)
}

/// Reads one complete message. Blocks until all of it is there.
pub fn read_message(pipe: &Pipe) -> anyhow::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    pipe.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..5].try_into().expect("4 bytes")) as usize;
    ensure!(
        len <= MAX_MESSAGE,
        "secure desktop message too large ({len})"
    );
    let mut payload = vec![0u8; len];
    pipe.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// One newline-terminated JSON line (the control pipe).
pub fn write_line(pipe: &Pipe, line: &str) -> anyhow::Result<()> {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    pipe.write_all(&buf)
}

pub fn read_line(pipe: &Pipe) -> anyhow::Result<String> {
    let mut out = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if pipe.read(&mut byte)? == 0 {
            bail!("the pipe closed");
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
        ensure!(out.len() <= 64 * 1024, "control line too long");
    }
    String::from_utf8(out).context("control line is not UTF-8")
}

pub(crate) fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
    // HRESULT_FROM_WIN32: 0x8007_0000 | (code & 0xffff).
    error.code().0 as u32 == (0x8007_0000 | (code & 0xffff))
}

/// A security descriptor from an SDDL string, freed on drop.
pub(crate) struct Sd(PSECURITY_DESCRIPTOR);

impl Sd {
    pub(crate) fn new(sddl: &str) -> anyhow::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL string is NUL-terminated by HSTRING; `sd` receives an allocation
        // made by LocalAlloc, released in Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("building a security descriptor")?;
        Ok(Sd(sd))
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }
}

fn config_payload(cfg: VideoConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&cfg.width.to_le_bytes());
    out.extend_from_slice(&cfg.height.to_le_bytes());
    out.extend_from_slice(&cfg.fps.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// client side

/// Shared state of the secure-desktop helper: whether the service is installed, and the data
/// pipe when a helper is running. One per agent; a second viewer session would find it busy.
pub struct SecureDesktop {
    state: AtomicU8,
    active: Mutex<Option<Active>>,
}

struct Active {
    pipe: Arc<Pipe>,
    /// Kept open so the service knows the agent is alive; closing it stops the helper.
    control: Pipe,
}

impl SecureDesktop {
    /// Checks whether the service is there. The helper itself starts on demand.
    pub fn new() -> Arc<Self> {
        // SAFETY: a presence check; it does not connect.
        let present = unsafe {
            windows::Win32::System::Pipes::WaitNamedPipeW(&HSTRING::from(CONTROL_PIPE), 500)
        }
        .as_bool();
        Self::with_present(present)
    }

    fn with_present(present: bool) -> Arc<Self> {
        if present {
            info!("secure desktop service found");
        } else {
            debug!("no secure desktop service; the lock screen stays in the last picture");
        }
        Arc::new(SecureDesktop {
            state: AtomicU8::new(if present { 1 } else { 0 }),
            active: Mutex::new(None),
        })
    }

    pub fn state(&self) -> SecureState {
        let raw = self.state.load(Ordering::Relaxed);
        // A failed attempt only applies to the locked period that tried it; after an unlock,
        // make the helper available again for the next lock.
        if raw == 3 && crate::session::lock_state() != rd_agent_core::LockState::Locked {
            let next = if service_present() { 1 } else { 0 };
            self.set_state(next);
            return if next == 1 {
                SecureState::Starting
            } else {
                SecureState::Unavailable
            };
        }
        match raw {
            1 => SecureState::Starting,
            2 => SecureState::Ready,
            3 => SecureState::Failed,
            _ => SecureState::Unavailable,
        }
    }

    pub(crate) fn set_state(&self, state: u8) {
        self.state.store(state, Ordering::Relaxed);
    }

    pub fn input_ready(&self) -> bool {
        self.state() == SecureState::Ready && self.active.lock().is_some()
    }

    /// Creates the data pipe, asks the service for a helper, and waits for it to connect.
    /// Runs on the pipeline's blocking thread.
    pub fn start(&self, cfg: VideoConfig) -> anyhow::Result<Arc<Pipe>> {
        ensure!(
            self.active.lock().is_none(),
            "a secure desktop helper is already running"
        );
        let name = format!(
            "{DATA_PIPE_PREFIX}{}",
            hex::encode(rand::random::<[u8; 16]>())
        );

        // SYSTEM only: the helper runs as SYSTEM; no user process can connect.
        let sd = Sd::new("D:(A;;GA;;;SY)")?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: the SDDL is valid; the pipe handle is owned by `pipe`.
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(name.as_str()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                1 << 20,
                0,
                Some(&sa),
            )
        };
        if handle == windows::Win32::Foundation::INVALID_HANDLE_VALUE {
            bail!("creating the secure desktop pipe failed");
        }
        let pipe = Arc::new(Pipe::from_handle(handle));

        // Ask the service to launch the helper. The control connection stays open: when it
        // closes, the service terminates the helper.
        let control = match Pipe::open(CONTROL_PIPE) {
            Ok(p) => p,
            Err(e) => {
                self.set_state(3);
                return Err(e.context("the secure desktop service is not running"));
            }
        };
        let request = serde_json::to_string(&SecureRequest::Start { pipe: name.clone() })?;
        if let Err(e) = write_line(&control, &request) {
            self.set_state(3);
            return Err(e);
        }
        let reply = read_line(&control).context("waiting for the secure desktop service")?;
        match serde_json::from_str::<SecureReply>(&reply)? {
            SecureReply::Ok => {}
            SecureReply::Error { msg } => {
                self.set_state(3);
                bail!("the secure desktop service refused: {msg}");
            }
        }

        // The helper connects as SYSTEM; it may take a moment to start.
        let raw = pipe.handle().0 as isize;
        let (tx, rx) = std::sync::mpsc::channel();
        let _connect = std::thread::Builder::new()
            .name("rd-secure-connect".into())
            .spawn(move || {
                // SAFETY: the handle is open until the main thread drops `pipe`.
                let result =
                    unsafe { ConnectNamedPipe(HANDLE(raw as *mut std::ffi::c_void), None) };
                let _ = tx.send(result);
            })?;
        let connected = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => is_win32_error(&e, ERROR_PIPE_CONNECTED.0),
            Err(_) => false,
        };
        if !connected {
            drop(pipe);
            self.set_state(3);
            let _ = write_line(
                &control,
                &serde_json::to_string(&SecureRequest::Stop).unwrap_or_default(),
            );
            bail!("the secure desktop helper did not connect");
        }

        if let Err(e) = write_message(&pipe, MSG_CONFIG, &config_payload(cfg)) {
            self.set_state(3);
            return Err(e);
        }
        // The helper sends READY after it has opened the secure desktop. Waiting for it here
        // means a helper that cannot reach Winlogon fails the pipeline (and the runtime falls
        // back) instead of leaving a frozen picture.
        let ready = read_message(&pipe).context("waiting for the secure desktop helper");
        match ready {
            Ok((MSG_READY, _)) => {}
            Ok((kind, _)) => {
                self.set_state(3);
                bail!("unexpected secure desktop message {kind} before ready");
            }
            Err(e) => {
                self.set_state(3);
                return Err(e);
            }
        }
        *self.active.lock() = Some(Active {
            pipe: pipe.clone(),
            control,
        });
        self.set_state(2);
        info!("secure desktop connected");
        Ok(pipe)
    }

    pub fn stop(&self) {
        let active = self.active.lock().take();
        if let Some(active) = active {
            let _ = write_message(&active.pipe, MSG_STOP, &[]);
            drop(active.pipe);
            if let Ok(line) = serde_json::to_string(&SecureRequest::Stop) {
                let _ = write_line(&active.control, &line);
            }
            info!("secure desktop stopped");
        }
        // The service is still installed; the next lock can start a new helper.
        self.set_state(if service_present() { 1 } else { 0 });
    }

    pub fn send_config(&self, cfg: VideoConfig) -> anyhow::Result<()> {
        let active = self.active.lock();
        let Some(active) = active.as_ref() else {
            bail!("the secure desktop is not running");
        };
        write_message(&active.pipe, MSG_CONFIG, &config_payload(cfg))
    }

    pub fn send_input(&self, wire: &[u8]) -> anyhow::Result<()> {
        let active = self.active.lock();
        let Some(active) = active.as_ref() else {
            bail!("the secure desktop is not running");
        };
        write_message(&active.pipe, MSG_INPUT, wire)
    }

    pub fn send_sas(&self) -> anyhow::Result<()> {
        if !software_sas_allowed() {
            bail!(
                "Ctrl+Alt+Del is disabled by policy: set \
                 HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System\\\
                 SoftwareSASGeneration to 1 (services) or 3"
            );
        }
        let active = self.active.lock();
        let Some(active) = active.as_ref() else {
            bail!("the secure desktop is not running");
        };
        write_message(&active.pipe, MSG_SAS, &[])
    }
}

fn service_present() -> bool {
    // SAFETY: a presence check; it does not connect.
    unsafe { windows::Win32::System::Pipes::WaitNamedPipeW(&HSTRING::from(CONTROL_PIPE), 200) }
        .as_bool()
}

/// `SoftwareSASGeneration`: bit 0 allows services, bit 1 applications. Absent or 0 means no
/// software-generated Ctrl+Alt+Del.
fn software_sas_allowed() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    // SAFETY: `value`/`size` describe a writable DWORD.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"),
            &HSTRING::from("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    rc == ERROR_SUCCESS && value & 1 != 0
}

// ---------------------------------------------------------------------------
// video pipeline

/// Capture the secure desktop via the helper and encode it with the software encoder (the
/// helper sends raw BGRA; there is no DXGI on the Winlogon desktop).
pub struct SecurePipeline {
    desktop: Arc<SecureDesktop>,
    bitrate: Arc<AtomicU32>,
    failure: Arc<Mutex<Option<String>>>,
    /// The next size/fps the reader thread should apply.
    reconfig: Arc<Mutex<Option<VideoConfig>>>,
    run: Option<Run>,
}

struct Run {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl SecurePipeline {
    pub fn new(desktop: Arc<SecureDesktop>) -> Self {
        SecurePipeline {
            desktop,
            bitrate: Arc::new(AtomicU32::new(0)),
            failure: Arc::new(Mutex::new(None)),
            reconfig: Arc::new(Mutex::new(None)),
            run: None,
        }
    }

    fn spawn_reader(
        &self,
        cfg: VideoConfig,
        gate: Arc<dyn rd_agent_core::platform::FrameGate>,
        sink: rd_agent_core::platform::FrameSink,
    ) -> anyhow::Result<Run> {
        let pipe = self.desktop.start(cfg)?;
        let bitrate = self.bitrate.clone();
        let failure = self.failure.clone();
        let reconfig = self.reconfig.clone();
        let desktop = self.desktop.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let gate2 = gate.clone();
        let sink2 = sink.clone();
        let thread = std::thread::Builder::new()
            .name("rd-secure-video".into())
            .spawn(move || {
                let _com = crate::mf::ComThread::init();
                let mut cfg = cfg;
                let mut image = crate::encoder::Nv12Image::new(cfg.width, cfg.height);
                let mut encoder = match crate::encoder::H264Encoder::new(
                    crate::capture::encoder_config(
                        &cfg,
                        crate::encoder::EncoderPreference::Software,
                    ),
                    None,
                    gate2.clone(),
                    sink2.clone(),
                    bitrate.clone(),
                ) {
                    Ok(encoder) => encoder,
                    Err(e) => {
                        *failure.lock() = Some(format!("opening the secure encoder: {e:#}"));
                        return;
                    }
                };
                let mut last_submit = Instant::now();
                let mut last_capture_us = 0u64;
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(new) = reconfig.lock().take() {
                        cfg = new;
                        image = crate::encoder::Nv12Image::new(cfg.width, cfg.height);
                        match crate::encoder::H264Encoder::new(
                            crate::capture::encoder_config(
                                &cfg,
                                crate::encoder::EncoderPreference::Software,
                            ),
                            None,
                            gate2.clone(),
                            sink2.clone(),
                            bitrate.clone(),
                        ) {
                            Ok(new_encoder) => {
                                // The old encoder drains its last frames when it is dropped.
                                encoder = new_encoder;
                            }
                            Err(e) => {
                                *failure.lock() =
                                    Some(format!("reopening the secure encoder: {e:#}"));
                                break;
                            }
                        }
                    }
                    let now = Instant::now();
                    encoder.maintain(now);
                    if let Some(fatal) = encoder.fatal() {
                        *failure.lock() = Some(fatal.to_owned());
                        break;
                    }
                    // A static lock screen still needs a keyframe after a resume/new viewer.
                    if last_capture_us != 0
                        && last_submit.elapsed() >= Duration::from_millis(200)
                        && (gate2.refresh_wanted() || encoder.keyframe_pending())
                    {
                        let capture_us = now_us();
                        if let Some(admitted) = gate2.admit(capture_us) {
                            let _ = encoder.encode_image(
                                &image,
                                capture_us,
                                admitted.seq,
                                admitted.force_keyframe,
                            );
                        }
                        last_submit = Instant::now();
                    }
                    match pipe.peek() {
                        Ok(0) => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if !stop2.load(Ordering::Relaxed) {
                                *failure.lock() = Some(format!("secure desktop pipe: {e:#}"));
                            }
                            break;
                        }
                    }
                    let (kind, payload) = match read_message(&pipe) {
                        Ok(message) => message,
                        Err(e) => {
                            if !stop2.load(Ordering::Relaxed) {
                                *failure.lock() = Some(format!("secure desktop pipe: {e:#}"));
                            }
                            break;
                        }
                    };
                    match kind {
                        MSG_FRAME => {
                            if payload.len() < 16 {
                                *failure.lock() = Some("short secure desktop frame".into());
                                break;
                            }
                            // The helper's clock is a different process origin; timestamp the
                            // frame on the agent's clock so viewer latency stays meaningful.
                            let _helper_us =
                                u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
                            let capture_us = now_us();
                            let width =
                                u32::from_le_bytes(payload[8..12].try_into().expect("4 bytes"));
                            let height =
                                u32::from_le_bytes(payload[12..16].try_into().expect("4 bytes"));
                            let bgra = &payload[16..];
                            if (width, height) != (cfg.width, cfg.height) {
                                // A frame from before a reconfigure; the helper's next frame
                                // has the new size.
                                debug!(
                                    width,
                                    height,
                                    want_width = cfg.width,
                                    want_height = cfg.height,
                                    "dropping a stale secure desktop frame"
                                );
                                continue;
                            }
                            if bgra.len() != (width as usize * height as usize * 4) {
                                *failure.lock() = Some("short secure desktop pixels".into());
                                break;
                            }
                            let Some(admitted) = gate2.admit(capture_us) else {
                                continue;
                            };
                            if let Err(e) = nv12::bgra_to_nv12(
                                bgra,
                                width as usize * 4,
                                width,
                                height,
                                &mut image.planes(),
                            ) {
                                debug!("secure desktop conversion failed: {e:#}");
                                gate2.cancel(admitted.seq);
                                continue;
                            }
                            if let Err(e) = encoder.encode_image(
                                &image,
                                capture_us,
                                admitted.seq,
                                admitted.force_keyframe,
                            ) {
                                debug!("secure desktop encode failed: {e:#}");
                            }
                            last_capture_us = capture_us;
                            last_submit = Instant::now();
                        }
                        MSG_ERROR => {
                            let msg = String::from_utf8_lossy(&payload).into_owned();
                            *failure.lock() = Some(msg);
                            break;
                        }
                        MSG_READY => debug!("secure desktop helper ready"),
                        other => debug!(kind = other, "unknown secure desktop message"),
                    }
                }
                if failure.lock().is_some() {
                    desktop.set_state(3);
                }
            })?;
        Ok(Run { stop, thread })
    }
}

impl VideoPipeline for SecurePipeline {
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn rd_agent_core::platform::FrameGate>,
        sink: rd_agent_core::platform::FrameSink,
    ) -> anyhow::Result<()> {
        self.stop();
        crate::capture::validate(&cfg)?;
        self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        *self.failure.lock() = None;
        let run = self.spawn_reader(cfg, gate, sink)?;
        self.run = Some(run);
        Ok(())
    }

    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        crate::capture::validate(&cfg)?;
        self.bitrate.store(cfg.bitrate_bps, Ordering::Relaxed);
        *self.reconfig.lock() = Some(cfg);
        let _ = self.desktop.send_config(cfg);
        Ok(())
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        if let Some(run) = self.run.take() {
            run.stop.store(true, Ordering::Relaxed);
            self.desktop.stop();
            let _ = run.thread.join();
        }
    }

    fn failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }
}

impl Drop for SecurePipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// input

/// Forwards input to the helper while the secure desktop is up; the normal [`SendInput`]
/// injector otherwise. The runtime's lock check calls [`Platform::secure_input_ready`] before
/// dropping input, so while the helper is active these events arrive here.
pub struct SecureInputInjector {
    desktop: Arc<SecureDesktop>,
    normal: crate::input::SendInputInjector,
}

impl SecureInputInjector {
    pub fn new(
        desktop: Arc<SecureDesktop>,
        geometry: Arc<crate::geometry::CaptureGeometry>,
    ) -> anyhow::Result<Self> {
        Ok(SecureInputInjector {
            desktop,
            normal: crate::input::SendInputInjector::new()?.with_capture_geometry(geometry),
        })
    }
}

impl InputInjector for SecureInputInjector {
    fn inject(&mut self, ev: InputEvent) {
        if self.desktop.input_ready() {
            let mut wire = Vec::with_capacity(16);
            ev.write(&mut wire);
            if self.desktop.send_input(&wire).is_ok() {
                return;
            }
            warn!("secure desktop input failed; falling back to the local session");
        }
        self.normal.inject(ev);
    }

    fn release_all(&mut self) {
        if self.desktop.input_ready() {
            let mut wire = Vec::new();
            InputEvent::ReleaseAll.write(&mut wire);
            if self.desktop.send_input(&wire).is_ok() {
                return;
            }
        }
        self.normal.release_all();
    }
}
