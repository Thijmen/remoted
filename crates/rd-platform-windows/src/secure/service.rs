//! The rd-agent-secure service: LocalSystem, starts at boot, and launches the secure-desktop
//! helper into the console session when the agent asks for it.
//!
//! Installed once with `rd-agent secure install` (administrator). The service's control pipe
//! allows interactive users; it verifies that the caller runs in the active console session as
//! that session's user before it starts anything. The helper itself runs as SYSTEM in the
//! console session and connects to a data pipe whose ACL allows SYSTEM only.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use tracing::{error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_EXISTS, HANDLE, LUID, NO_ERROR,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, EqualSid, GetTokenInformation, LUID_AND_ATTRIBUTES,
    LookupPrivilegeValueW, PSID, SE_ASSIGNPRIMARYTOKEN_NAME, SE_INCREASE_QUOTA_NAME,
    SE_PRIVILEGE_ENABLED, SE_TCB_NAME, SecurityImpersonation, SetTokenInformation,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenPrimary, TokenSessionId, TokenUser,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, RegisterServiceCtrlHandlerExW,
    SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTUPINFOW, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{WTS_SESSION_LOGOFF, WTS_SESSION_UNLOCK};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use super::{
    CONTROL_PIPE, Pipe, SERVICE_NAME, Sd, SecureReply, SecureRequest, is_win32_error, read_line,
    write_line,
};

/// The service's control pipe handle; the service main closes it on stop.
static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
/// `SetServiceStatus` needs the handle from any thread; keep it as an isize.
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

pub struct Service {
    stop: AtomicBool,
    helper: Mutex<Option<Helper>>,
}

struct Helper {
    process: HANDLE,
}

// SAFETY: HANDLE is a kernel object; only `stop_helper` (under the mutex) uses it.
unsafe impl Send for Helper {}

impl Service {
    fn new() -> Arc<Self> {
        Arc::new(Service {
            stop: AtomicBool::new(false),
            helper: Mutex::new(None),
        })
    }

    fn stop_helper(&self) {
        if let Some(helper) = self.helper.lock().take() {
            // SAFETY: the handle is owned by Helper and closed below.
            unsafe { TerminateProcess(helper.process, 1) }.ok();
            // SAFETY: closes the only handle to the helper process.
            unsafe { CloseHandle(helper.process) }.ok();
        }
    }

    fn spawn_helper(&self, session: u32, pipe_name: &str) -> anyhow::Result<()> {
        self.stop_helper();
        let exe = std::env::current_exe()
            .context("the service has no path")?
            .with_file_name("rd-agent-sec.exe");
        ensure!(exe.is_file(), "{} is missing", exe.display());
        enable_privilege(SE_ASSIGNPRIMARYTOKEN_NAME)?;
        enable_privilege(SE_INCREASE_QUOTA_NAME)?;
        enable_privilege(SE_TCB_NAME)?;

        let mut token = HANDLE::default();
        // SAFETY: the current process token is opened for duplication and closed below.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) }
            .context("opening the service token")?;
        let mut primary = HANDLE::default();
        // SAFETY: a SYSTEM primary token; closed below.
        let dup = unsafe {
            DuplicateTokenEx(
                token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        };
        // SAFETY: the token handle is no longer needed.
        unsafe { CloseHandle(token) }.ok();
        dup.context("duplicating the service token")?;

        // Run in the console session, not session 0.
        // SAFETY: `session` is a u32; the token is a primary token.
        let set = unsafe {
            SetTokenInformation(
                primary,
                TokenSessionId,
                &session as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            )
        };
        if let Err(e) = set {
            // SAFETY: closes the duplicated token.
            unsafe { CloseHandle(primary) }.ok();
            return Err(e).context("moving the helper token to the console session");
        }

        let app = HSTRING::from(exe.to_string_lossy().as_ref());
        let mut cmd: Vec<u16> = format!("\"{}\" --pipe \"{}\"", exe.display(), pipe_name)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: the command line is mutable and NUL-terminated; the token is primary and in
        // the console session; the process handle is stored below.
        let created = unsafe {
            CreateProcessAsUserW(
                Some(primary),
                &app,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut process,
            )
        };
        // SAFETY: the token and the thread handle are no longer needed.
        unsafe { CloseHandle(primary) }.ok();
        // SAFETY: the thread handle is not used.
        unsafe { CloseHandle(process.hThread) }.ok();
        created.context("starting the secure desktop helper")?;

        info!(
            pid = process.dwProcessId,
            session, "secure desktop helper started"
        );
        *self.helper.lock() = Some(Helper {
            process: process.hProcess,
        });
        Ok(())
    }

    fn serve(self: Arc<Self>) -> anyhow::Result<()> {
        let sd = Sd::new("D:(A;;GA;;;SY)(A;;GA;;;IU)")?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: the SDDL is valid; the handle is owned by `pipe`.
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(CONTROL_PIPE),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        if handle == windows::Win32::Foundation::INVALID_HANDLE_VALUE {
            bail!("creating the secure desktop control pipe failed");
        }
        let pipe = Pipe::from_handle(handle);
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let connected = match unsafe { ConnectNamedPipe(pipe.handle(), None) } {
                Ok(()) => true,
                Err(e) => is_win32_error(&e, ERROR_PIPE_CONNECTED.0),
            };
            if !connected {
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if let Err(e) = self.client(&pipe) {
                warn!("secure desktop control client: {e:#}");
            }
            // SAFETY: the client has gone; the pipe can accept a new one.
            unsafe { DisconnectNamedPipe(pipe.handle()) }.ok();
        }
        Ok(())
    }

    fn client(&self, pipe: &Pipe) -> anyhow::Result<()> {
        let session = match authorize(pipe.handle()) {
            Ok(session) => session,
            Err(e) => {
                reply(pipe, Err(format!("{e:#}")));
                return Ok(());
            }
        };
        loop {
            let line = match read_line(pipe) {
                Ok(line) => line,
                Err(_) => break, // the agent closed the control connection
            };
            let request: SecureRequest = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(e) => {
                    reply(pipe, Err(format!("invalid request: {e}")));
                    continue;
                }
            };
            match request {
                SecureRequest::Start { pipe: name } => {
                    if let Err(e) = valid_data_pipe(&name) {
                        reply(pipe, Err(format!("{e:#}")));
                        continue;
                    }
                    match self.spawn_helper(session, &name) {
                        Ok(()) => reply(pipe, Ok(())),
                        Err(e) => reply(pipe, Err(format!("{e:#}"))),
                    }
                }
                SecureRequest::Stop => {
                    self.stop_helper();
                    reply(pipe, Ok(()));
                    break;
                }
            }
        }
        // The agent is gone (or asked us to stop): never leave a SYSTEM helper behind.
        self.stop_helper();
        Ok(())
    }
}

fn reply(pipe: &Pipe, result: Result<(), String>) {
    let reply = match result {
        Ok(()) => SecureReply::Ok,
        Err(msg) => {
            warn!("refusing secure desktop request: {msg}");
            SecureReply::Error { msg }
        }
    };
    if let Ok(line) = serde_json::to_string(&reply) {
        let _ = write_line(pipe, &line);
    }
}

/// Only our own data pipes: `\\.\pipe\rd-secure-` and 32 hex characters. The name ends up on a
/// SYSTEM command line, so nothing else may pass.
fn valid_data_pipe(name: &str) -> anyhow::Result<()> {
    let token = name
        .strip_prefix(super::DATA_PIPE_PREFIX)
        .context("not an rd secure desktop pipe")?;
    ensure!(
        token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid secure desktop pipe name"
    );
    Ok(())
}

/// The caller must be the console session's user: that is the only session whose lock screen
/// this service may expose.
fn authorize(pipe: HANDLE) -> anyhow::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: the pipe handle is valid for this connection.
    unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.context("the pipe has no client")?;
    let mut session = 0u32;
    // SAFETY: `pid` comes from the pipe.
    unsafe { ProcessIdToSessionId(pid, &mut session) }.context("the client has no session")?;
    // SAFETY: no preconditions; returns u32::MAX when there is no console session.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    ensure!(
        console != u32::MAX && session == console,
        "only the console session may use the secure desktop"
    );

    let mut user = HANDLE::default();
    // SAFETY: the service is SYSTEM (SeTcbPrivilege); the token is closed below.
    unsafe { WTSQueryUserToken(session, &mut user) }.context("WTSQueryUserToken")?;
    let user_sid = match token_sid(user) {
        Ok(sid) => sid,
        Err(e) => {
            // SAFETY: closes the token.
            unsafe { CloseHandle(user) }.ok();
            return Err(e);
        }
    };
    // SAFETY: the token is no longer needed.
    unsafe { CloseHandle(user) }.ok();

    // SAFETY: the client process is queried and closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .context("opening the client process")?;
    let mut process_token = HANDLE::default();
    // SAFETY: opens the client's token; closed below.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut process_token) };
    // SAFETY: the process handle is no longer needed.
    unsafe { CloseHandle(process) }.ok();
    opened.context("opening the client token")?;
    let client_sid = token_sid(process_token);
    // SAFETY: the token is no longer needed.
    unsafe { CloseHandle(process_token) }.ok();
    let client_sid = client_sid?;

    // SAFETY: both pointers point at TOKEN_USER structures in live buffers.
    let same = unsafe { EqualSid(sid_of(&user_sid), sid_of(&client_sid)) }.is_ok();
    ensure!(same, "the client is not the console user");
    Ok(session)
}

/// A `TOKEN_USER` buffer, 8-byte aligned.
fn token_sid(token: HANDLE) -> anyhow::Result<Vec<u64>> {
    let mut needed = 0u32;
    // SAFETY: a size query; the expected error is ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    ensure!(needed > 0, "the token has no user");
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` is writable for `needed` bytes (and more).
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .context("GetTokenInformation(TokenUser)")?;
    Ok(buf)
}

fn sid_of(buf: &[u64]) -> PSID {
    // SAFETY: `token_sid` filled this buffer with a TOKEN_USER.
    let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    user.User.Sid
}

// ---------------------------------------------------------------------------
// service entry point

/// Runs as the service (SCM) or, with `--console`, in the foreground for debugging.
pub fn run() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--console") {
        let service = Service::new();
        let _ = SERVICE.set(service.clone());
        return tokio_less_serve(service);
    }
    let mut name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table is NUL-terminated and lives for the call.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .context("StartServiceCtrlDispatcher (run this through the service manager)")?;
    Ok(())
}

fn tokio_less_serve(service: Arc<Service>) -> anyhow::Result<()> {
    info!("secure desktop service (console mode)");
    service.serve()
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    // SAFETY: called by the SCM on a service thread.
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(&HSTRING::from(SERVICE_NAME), Some(handler), None)
    } {
        Ok(handle) => handle,
        Err(_) => return,
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::Release);
    set_status(SERVICE_START_PENDING, 0, 3000);

    let service = Service::new();
    let _ = SERVICE.set(service.clone());
    // WTSQueryUserToken needs SeTcbPrivilege; SYSTEM usually has it enabled, but be explicit.
    let _ = enable_privilege(SE_TCB_NAME);
    set_status(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
    );

    let serving = service.clone();
    let _ = std::thread::Builder::new()
        .name("rd-secure-control".into())
        .spawn(move || {
            if let Err(e) = serving.serve() {
                error!("secure desktop control pipe: {e:#}");
            }
        });

    while !service.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    service.stop_helper();
    set_status(SERVICE_STOPPED, 0, 0);
}

unsafe extern "system" fn handler(
    control: u32,
    event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if let Some(service) = SERVICE.get() {
        match control {
            SERVICE_CONTROL_STOP | windows::Win32::System::Services::SERVICE_CONTROL_SHUTDOWN => {
                service.stop.store(true, Ordering::Release);
                service.stop_helper();
            }
            windows::Win32::System::Services::SERVICE_CONTROL_SESSIONCHANGE
                if event == WTS_SESSION_UNLOCK || event == WTS_SESSION_LOGOFF =>
            {
                service.stop_helper();
            }
            _ => {}
        }
    }
    NO_ERROR.0
}

fn set_status(
    state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
    hint: u32,
) {
    let raw = STATUS_HANDLE.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: hint,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW.
    unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(raw as *mut c_void), &status) }.ok();
}

// ---------------------------------------------------------------------------
// install / uninstall / status

pub fn install() -> anyhow::Result<()> {
    ensure_admin()?;
    let source = std::env::current_exe()
        .context("rd-agent has no path")?
        .parent()
        .context("rd-agent has no folder")?
        .to_path_buf();
    let dest = program_files()?.join("rd-agent");
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    // Stop the old service before replacing its binary.
    stop_service().ok();
    for file in ["rd-agent-svc.exe", "rd-agent-sec.exe"] {
        let from = source.join(file);
        ensure!(
            from.is_file(),
            "{} is missing (run the installer)",
            from.display()
        );
        std::fs::copy(&from, dest.join(file))
            .with_context(|| format!("copying {file} to {}", dest.display()))?;
    }

    // Quote the path: it lives under "Program Files", and an unquoted service path is the
    // classic unquoted-service-path weakness.
    let binary = format!("\"{}\"", dest.join("rd-agent-svc.exe").display());
    let binary = HSTRING::from(binary);
    // SAFETY: the SCM handles are opened and closed in this block.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) }
        .context("opening the service manager (run as administrator)")?;
    let created = unsafe {
        CreateServiceW(
            scm,
            &HSTRING::from(SERVICE_NAME),
            &HSTRING::from("rd secure desktop"),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            &binary,
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };
    let service = match created {
        Ok(service) => service,
        Err(e) if is_win32_error(&e, ERROR_SERVICE_EXISTS.0) => {
            let service =
                unsafe { OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) }
                    .context("opening the existing service")?;
            // SAFETY: updates the binary path/start type; the service is not running.
            unsafe {
                ChangeServiceConfigW(
                    service,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &binary,
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    &HSTRING::from("rd secure desktop"),
                )
            }
            .context("updating the existing service")?;
            service
        }
        Err(e) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            return Err(e).context("creating the service");
        }
    };
    // SAFETY: the service handle is open.
    let started = unsafe { windows::Win32::System::Services::StartServiceW(service, None) };
    if let Err(e) = started
        && !is_win32_error(&e, ERROR_SERVICE_ALREADY_RUNNING.0)
    {
        // SAFETY: closes the handles.
        unsafe { CloseServiceHandle(service) }.ok();
        // SAFETY: closes the manager handle.
        unsafe { CloseServiceHandle(scm) }.ok();
        return Err(e).context("starting the service");
    }
    // SAFETY: closes the handles.
    unsafe { CloseServiceHandle(service) }.ok();
    // SAFETY: closes the manager handle.
    unsafe { CloseServiceHandle(scm) }.ok();
    println!(
        "rd secure desktop installed; it starts at boot. Restart the agent (rd-agent install) \
         so it finds the service, then lock the PC: the viewer shows the lock screen."
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    ensure_admin()?;
    stop_service().ok();
    // SAFETY: the manager handle is closed below.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) }
        .context("opening the service manager (run as administrator)")?;
    let service = unsafe { OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) };
    match service {
        Ok(service) => {
            // SAFETY: the service is stopped; deletes its registration.
            let deleted = unsafe { DeleteService(service) };
            // SAFETY: closes the service handle.
            unsafe { CloseServiceHandle(service) }.ok();
            if let Err(e) = deleted
                && !is_win32_error(&e, ERROR_SERVICE_DOES_NOT_EXIST.0)
            {
                // SAFETY: closes the manager handle.
                unsafe { CloseServiceHandle(scm) }.ok();
                return Err(e).context("deleting the service");
            }
        }
        Err(e) if is_win32_error(&e, ERROR_SERVICE_DOES_NOT_EXIST.0) => {}
        Err(e) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            return Err(e).context("opening the service");
        }
    }
    // SAFETY: closes the manager handle.
    unsafe { CloseServiceHandle(scm) }.ok();
    if let Ok(dir) = program_files().map(|p| p.join("rd-agent")) {
        let _ = std::fs::remove_dir_all(dir);
    }
    println!("rd secure desktop uninstalled");
    Ok(())
}

/// Whether the service is registered (running or not). Used by `rd-agent uninstall` to warn.
pub fn installed() -> bool {
    // SAFETY: handles are closed below.
    let Ok(scm) = (unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) })
    else {
        return false;
    };
    let service = unsafe { OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_QUERY_STATUS) };
    let installed = service.is_ok();
    if let Ok(service) = service {
        // SAFETY: closes the service handle.
        unsafe { CloseServiceHandle(service) }.ok();
    }
    // SAFETY: closes the manager handle.
    unsafe { CloseServiceHandle(scm) }.ok();
    installed
}

pub fn status() -> anyhow::Result<()> {
    // SAFETY: handles are closed below.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .context("opening the service manager")?;
    let service = unsafe { OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_QUERY_STATUS) };
    let service = match service {
        Ok(service) => service,
        Err(e) if is_win32_error(&e, ERROR_SERVICE_DOES_NOT_EXIST.0) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            println!("rd secure desktop is not installed");
            return Ok(());
        }
        Err(e) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            return Err(e).context("opening the service");
        }
    };
    let mut buf = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
    let mut needed = 0u32;
    // SAFETY: the buffer is large enough; the API fills it.
    unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed) }
        .context("querying the service")?;
    // SAFETY: the API wrote a SERVICE_STATUS_PROCESS.
    let status = unsafe { &*(buf.as_ptr().cast::<SERVICE_STATUS_PROCESS>()) };
    let state = match status.dwCurrentState {
        SERVICE_RUNNING => "running",
        SERVICE_STOPPED => "stopped",
        _ => "starting/stopping",
    };
    println!("rd secure desktop: {state} (pid {})", status.dwProcessId);
    // SAFETY: closes the handles.
    unsafe { CloseServiceHandle(service) }.ok();
    // SAFETY: closes the manager handle.
    unsafe { CloseServiceHandle(scm) }.ok();
    Ok(())
}

fn stop_service() -> anyhow::Result<()> {
    // SAFETY: handles are closed below.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .context("opening the service manager")?;
    let service = unsafe {
        OpenServiceW(
            scm,
            &HSTRING::from(SERVICE_NAME),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        )
    };
    let service = match service {
        Ok(service) => service,
        Err(e) if is_win32_error(&e, ERROR_SERVICE_DOES_NOT_EXIST.0) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            return Ok(());
        }
        Err(e) => {
            // SAFETY: closes the manager handle.
            unsafe { CloseServiceHandle(scm) }.ok();
            return Err(e).context("opening the service");
        }
    };
    let mut status = SERVICE_STATUS::default();
    // SAFETY: a stop request; the service handles it.
    unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) }.ok();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = query_status(service)
            .map(|status| status.dwCurrentState == SERVICE_STOPPED)
            .unwrap_or(false);
        if stopped || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: closes the handles.
    unsafe { CloseServiceHandle(service) }.ok();
    // SAFETY: closes the manager handle.
    unsafe { CloseServiceHandle(scm) }.ok();
    Ok(())
}

fn query_status(
    service: windows::Win32::System::Services::SC_HANDLE,
) -> anyhow::Result<SERVICE_STATUS_PROCESS> {
    let bytes = size_of::<SERVICE_STATUS_PROCESS>();
    // SERVICE_STATUS_PROCESS is 8-byte aligned; a [u64] buffer gives the cast a valid
    // alignment (the API writes at most `bytes` of it).
    let mut buf = [0u64; size_of::<SERVICE_STATUS_PROCESS>().div_ceil(8)];
    let mut needed = 0u32;
    let slice = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), bytes) };
    // SAFETY: the buffer is large enough; the API fills it.
    unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(slice), &mut needed) }
        .context("querying the service")?;
    // SAFETY: the API wrote a SERVICE_STATUS_PROCESS into the aligned buffer.
    Ok(unsafe { *buf.as_ptr().cast::<SERVICE_STATUS_PROCESS>() })
}

fn program_files() -> anyhow::Result<PathBuf> {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .context("ProgramFiles is not set")
}

fn ensure_admin() -> anyhow::Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: the current process token is opened and closed here.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .context("opening the process token")?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0u32;
    // SAFETY: `elevation` is a TOKEN_ELEVATION.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut needed,
        )
    };
    // SAFETY: closes the token.
    unsafe { CloseHandle(token) }.ok();
    read.context("reading the token elevation")?;
    ensure!(
        elevation.TokenIsElevated != 0,
        "this needs an administrator: run it from an elevated terminal"
    );
    Ok(())
}

fn enable_privilege(name: PCWSTR) -> anyhow::Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: the token is opened and closed here.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .context("opening the service token")?;
    let mut luid = LUID::default();
    // SAFETY: `luid` is writable.
    let lookup = unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) };
    if let Err(e) = lookup {
        // SAFETY: closes the token.
        unsafe { CloseHandle(token) }.ok();
        return Err(e).context("looking up a privilege");
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `privileges` describes one privilege on the token.
    let adjusted = unsafe { AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) };
    // SAFETY: closes the token.
    unsafe { CloseHandle(token) }.ok();
    adjusted.context("enabling a privilege")?;
    Ok(())
}
