//! The terminal's pseudo console (docs/terminal.md, Agent side): ConPTY, Windows 10 1809+.
//!
//! [`spawn`] creates a pseudo console on two pipes and starts the shell attached to it
//! (`PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`). The agent keeps the input pipe's write end and the
//! output pipe's read end; the console's ends are closed right after `CreatePseudoConsole`
//! (it holds its own copies).
//!
//! ConPTY pitfalls this follows:
//! - The output pipe stays open as long as the pseudo console does, even after the shell
//!   exited: [`PtyChild::wait`] closes it once the process is gone, so the reader gets the
//!   last output and then end of file.
//! - `ClosePseudoConsole` may block until the console's output is read (older builds): it is
//!   never called on the reader thread, and the runtime's reader keeps reading meanwhile.
//! - The functions are looked up at run time, so the agent still starts on a Windows without
//!   them (and then does not offer the terminal).
//! - The child gets explicitly invalid standard handles, so it never inherits the agent's
//!   (possibly redirected) ones instead of the console's.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rd_agent_core::{PtyChild, PtyExit, PtyProcess, PtyRequest, PtySize};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{COORD, HPCON};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::core::{HRESULT, PCWSTR, PWSTR, s, w};

type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type CloseFn = unsafe extern "system" fn(HPCON);

/// The ConPTY functions of kernel32.
struct Api {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: kernel32 is always loaded; the looked-up functions have these signatures
        // (consoleapi.h).
        unsafe {
            let kernel32 = GetModuleHandleW(w!("kernel32.dll")).ok()?;
            let create = GetProcAddress(kernel32, s!("CreatePseudoConsole"))?;
            let resize = GetProcAddress(kernel32, s!("ResizePseudoConsole"))?;
            let close = GetProcAddress(kernel32, s!("ClosePseudoConsole"))?;
            Some(Api {
                create: std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateFn>(
                    create,
                ),
                resize: std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizeFn>(
                    resize,
                ),
                close: std::mem::transmute::<unsafe extern "system" fn() -> isize, CloseFn>(close),
            })
        }
    })
    .as_ref()
}

/// This Windows has ConPTY (10 1809 or later).
pub fn supported() -> bool {
    api().is_some()
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: size.cols.min(i16::MAX as u16) as i16,
        Y: size.rows.min(i16::MAX as u16) as i16,
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn owned(h: HANDLE) -> OwnedHandle {
    // SAFETY: `h` is a handle this module just created and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(h.0) }
}

/// (read end, write end) of an anonymous pipe, not inheritable.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    // SAFETY: CreatePipe writes two new handles.
    unsafe { CreatePipe(&mut r, &mut w, None, 0) }.map_err(io::Error::other)?;
    Ok((owned(r), owned(w)))
}

/// The default shell: PowerShell 7 when it is on `PATH`, else Windows PowerShell.
fn default_shell() -> String {
    let pwsh = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("pwsh.exe"))
        // App execution aliases (a Store install) are reparse points `is_file` cannot follow.
        .find(|p| p.is_file() || std::fs::symlink_metadata(p).is_ok());
    let program = pwsh.unwrap_or_else(|| {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
    });
    quote(&program.to_string_lossy())
}

fn quote(program: &str) -> String {
    if program.contains(' ') && !program.starts_with('"') {
        format!("\"{program}\"")
    } else {
        program.to_owned()
    }
}

/// "pwsh" for `"C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo`.
fn shell_name(command_line: &str) -> String {
    let line = command_line.trim_start();
    let program = match line.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => line.split_whitespace().next().unwrap_or(line),
    };
    Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_owned())
}

/// The agent's environment, changed as `req` says, as a sorted Unicode environment block.
fn environment(req: &PtyRequest) -> Vec<u16> {
    let key = |k: &OsStr| k.to_string_lossy().to_uppercase();
    let mut vars: BTreeMap<String, (OsString, OsString)> = std::env::vars_os()
        .map(|(k, v)| (key(&k), (k, v)))
        .collect();
    for k in &req.env_remove {
        vars.remove(&k.to_uppercase());
    }
    for (k, v) in &req.env {
        vars.insert(k.to_uppercase(), (k.into(), v.into()));
    }
    let mut block = Vec::new();
    for (k, v) in vars.values() {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Starts `[terminal] shell` (a command line) or the default shell in a new pseudo console of
/// `req.size`, in `%USERPROFILE%`.
pub fn spawn(req: &PtyRequest) -> io::Result<PtyProcess> {
    let api = api().ok_or(io::ErrorKind::Unsupported)?;
    let command_line = req
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(default_shell, str::to_owned);

    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let mut hpc = HPCON::default();
    // SAFETY: valid pipe handles; the console duplicates them.
    let hr = unsafe {
        (api.create)(
            coord(req.size),
            HANDLE(raw(&in_read)),
            HANDLE(raw(&out_write)),
            0,
            &mut hpc,
        )
    };
    hr.ok().map_err(io::Error::other)?;
    let console = Arc::new(Console {
        api,
        hpc: Mutex::new(Some(Hpc(hpc))),
    });
    drop((in_read, out_write));

    let process = start_process(&command_line, hpc, &environment(req))?;
    Ok(PtyProcess {
        name: shell_name(&command_line),
        reader: Box::new(File::from(out_read)),
        writer: Box::new(File::from(in_write)),
        child: Arc::new(Child { process, console }),
    })
}

fn raw(h: &OwnedHandle) -> *mut c_void {
    use std::os::windows::io::AsRawHandle as _;
    h.as_raw_handle()
}

/// `CreateProcessW` of `command_line` attached to the pseudo console.
fn start_process(command_line: &str, hpc: HPCON, env: &[u16]) -> io::Result<OwnedHandle> {
    let mut size = 0usize;
    // SAFETY: the first call only reports the size (and fails with "insufficient buffer").
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
    // usize elements: the list must be pointer aligned.
    let mut list_buf = vec![0usize; size.div_ceil(size_of::<usize>()).max(1)];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buf.as_mut_ptr().cast());
    // SAFETY: `list_buf` has the size asked for and outlives the list.
    unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
        .map_err(io::Error::other)?;
    struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for ListGuard {
        fn drop(&mut self) {
            // SAFETY: initialized above, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.0) }
        }
    }
    let _list_guard = ListGuard(list);
    // SAFETY: for this attribute the value is the HPCON itself (consoleapi.h sample).
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(hpc.0 as *const c_void),
            size_of::<HPCON>(),
            None,
            None,
        )
    }
    .map_err(io::Error::other)?;

    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = list;

    let mut cmd = wide(OsStr::new(command_line));
    let cwd = rd_agent_core::home_dir()
        .filter(|h| h.is_dir())
        .map(|h| wide(h.as_os_str()));
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every buffer outlives the call; `cmd` is writable as CreateProcessW requires.
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            Some(env.as_ptr().cast()),
            cwd.as_ref().map_or(PCWSTR::null(), |c| PCWSTR(c.as_ptr())),
            &si.StartupInfo,
            &mut pi,
        )
    }
    .map_err(|e| io::Error::other(format!("starting {command_line}: {e}")))?;
    // SAFETY: the thread handle is ours and not needed.
    unsafe { CloseHandle(pi.hThread) }.ok();
    Ok(owned(pi.hProcess))
}

/// An HPCON that may move between threads (it is a plain kernel object reference).
#[derive(Clone, Copy)]
struct Hpc(HPCON);
// SAFETY: the pseudo console functions may be called from any thread.
unsafe impl Send for Hpc {}

struct Console {
    api: &'static Api,
    /// `None` once closed.
    hpc: Mutex<Option<Hpc>>,
}

impl Console {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        // Held during the call: `close` cannot free the console meanwhile. Resizing only
        // posts a message to the console, it does not wait for it.
        let hpc = self.hpc.lock();
        let Some(Hpc(hpc)) = *hpc else {
            return Ok(());
        };
        // SAFETY: a live pseudo console.
        unsafe { (self.api.resize)(hpc, coord(size)) }
            .ok()
            .map_err(io::Error::other)
    }

    /// Closes the console (once): the shell is told to end, the output pipe ends after the
    /// console's last output. May block until that output is read.
    fn close(&self) {
        let hpc = self.hpc.lock().take();
        if let Some(Hpc(hpc)) = hpc {
            // SAFETY: taken out of the slot, so closed exactly once.
            unsafe { (self.api.close)(hpc) }
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.close();
    }
}

struct Child {
    process: OwnedHandle,
    console: Arc<Console>,
}

impl Child {
    fn handle(&self) -> HANDLE {
        HANDLE(raw(&self.process))
    }
}

impl PtyChild for Child {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        self.console.resize(size)
    }

    fn hangup(&self) {
        self.console.close();
    }

    fn kill(&self) {
        // SAFETY: our process handle; fails harmlessly once the process exited.
        let _ = unsafe { TerminateProcess(self.handle(), 1) };
    }

    fn wait(&self) -> PtyExit {
        // SAFETY: waits on our process handle.
        let waited = unsafe { WaitForSingleObject(self.handle(), INFINITE) };
        let mut code = 0u32;
        // SAFETY: reads the exit code of our (exited) process.
        let got = waited == WAIT_OBJECT_0
            && unsafe { GetExitCodeProcess(self.handle(), &mut code) }.is_ok();
        // The output pipe ends only with the console: the reader gets the last output, then
        // end of file.
        self.console.close();
        PtyExit {
            code: got.then_some(code as i32),
            signal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_quotes() {
        assert_eq!(shell_name("pwsh.exe"), "pwsh");
        assert_eq!(
            shell_name(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#),
            "pwsh"
        );
        assert_eq!(shell_name(r"C:\Windows\System32\cmd.exe /k echo hi"), "cmd");
        assert_eq!(
            quote(r"C:\Program Files\x.exe"),
            r#""C:\Program Files\x.exe""#
        );
        assert_eq!(quote(r"C:\x.exe"), r"C:\x.exe");
    }

    #[test]
    fn environment_block() {
        let req = PtyRequest {
            size: PtySize { cols: 80, rows: 24 },
            shell: None,
            env: vec![("TERM_PROGRAM".into(), "rd".into())],
            env_remove: vec!["RD_AGENT_TOKEN".into()],
        };
        let block = environment(&req);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
        let text = String::from_utf16_lossy(&block);
        assert!(text.contains("TERM_PROGRAM=rd\0"));
        assert!(!text.to_uppercase().contains("RD_AGENT_TOKEN="));
    }
}
