//! Windows keyboard grab.
//!
//! A dedicated thread runs a message loop and owns a `WH_KEYBOARD_LL` hook. The hook is
//! installed when the page asks for the grab (`rd_set_grab({ active: true })`) while the main
//! window is the foreground window, and the request resolves only after that. While installed,
//! every physical key is swallowed (so Windows never acts on Alt+Tab, the Windows key, Caps
//! Lock, …) and forwarded to the page as `window.rdNative.key(code, down, repeat)`.
//!
//! The hook comes off when the page says so, or for the app's own reasons: the window lost the
//! foreground, the input desktop switched (lock screen, UAC, Ctrl+Alt+Del), a new page loads, or
//! the window closes. In the latter cases the page gets `window.rdNative.release()` and the app
//! waits for the page to ask again; it never re-installs the hook by itself.
//!
//! The hook procedure only does table lookups and a channel send: Windows silently removes
//! low-level hooks that are slow to return. Keys are never logged.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::Duration;

use tauri::WebviewWindow;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND,
    GetForegroundWindow, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, LLKHF_UP, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT, WM_APP, WM_USER,
};
use windows::core::PCWSTR;

use crate::keys;

/// Posted to the hook thread after each [`Command`] (wakes its message loop).
const WM_RD_COMMAND: u32 = WM_APP + 1;

/// Callers -> hook thread.
enum Command {
    SetGrab {
        active: bool,
        reply: SyncSender<Result<(), String>>,
    },
    /// The main window's focus changed: is it still the foreground window?
    Recheck,
    /// A new page is loading.
    Navigation,
    /// The window is closing: remove the hook, end the thread.
    Stop,
}

/// Hook thread -> forwarder thread.
enum Event {
    Key {
        code: &'static str,
        down: bool,
        repeat: bool,
    },
    Release,
}

/// Why the hook comes off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Page,
    Foreground,
    DesktopSwitch,
    Navigation,
    Stop,
}

/// Handle to the hook thread.
pub struct HookThread {
    thread_id: u32,
    commands: Sender<Command>,
}

impl HookThread {
    pub fn start(window: &WebviewWindow) -> Result<Self, String> {
        let hwnd = window
            .hwnd()
            .map_err(|e| format!("no window handle: {e}"))?
            .0 as isize;
        let (events_tx, events_rx) = mpsc::channel();
        let target = window.clone();
        std::thread::Builder::new()
            .name("rd-kbd-forward".into())
            .spawn(move || forward(target, events_rx))
            .map_err(|e| e.to_string())?;
        let (commands, commands_rx) = mpsc::channel();
        let (id_tx, id_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("rd-kbd-hook".into())
            .spawn(move || hook_thread(hwnd, events_tx, commands_rx, id_tx))
            .map_err(|e| e.to_string())?;
        let thread_id = id_rx
            .recv()
            .map_err(|_| "hook thread did not start".to_string())?;
        Ok(Self {
            thread_id,
            commands,
        })
    }

    /// Resolves once the hook is installed (`true`) or removed (`false`).
    pub async fn set_page_active(&self, active: bool) -> Result<(), String> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Command::SetGrab { active, reply })?;
        tauri::async_runtime::spawn_blocking(move || answer.recv_timeout(Duration::from_secs(2)))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|_| "keyboard grab: no answer from the hook thread".to_string())?
    }

    pub fn recheck_foreground(&self) {
        let _ = self.send(Command::Recheck);
    }

    pub fn navigation(&self) {
        let _ = self.send(Command::Navigation);
    }

    pub fn stop(&self) {
        let _ = self.send(Command::Stop);
    }

    fn send(&self, command: Command) -> Result<(), String> {
        let gone = || "keyboard grab: hook thread has stopped".to_string();
        self.commands.send(command).map_err(|_| gone())?;
        // SAFETY: plain FFI call; a thread that has exited only makes it fail.
        unsafe { PostThreadMessageW(self.thread_id, WM_RD_COMMAND, WPARAM(0), LPARAM(0)) }
            .map_err(|_| gone())
    }
}

/// Runs the page calls on the webview, in order. Never logs the script (it names keys).
fn forward(window: WebviewWindow, rx: Receiver<Event>) {
    for event in rx {
        let js = match event {
            Event::Key { code, down, repeat } => {
                // Codes come from the static key table; JSON-escape them anyway.
                let Ok(code) = serde_json::to_string(code) else {
                    continue;
                };
                format!("window.rdNative&&window.rdNative.key({code},{down},{repeat})")
            }
            Event::Release => "window.rdNative&&window.rdNative.release()".to_string(),
        };
        if let Err(e) = window.eval(js) {
            log::warn!("keyboard grab: cannot reach the page: {e}");
        }
    }
}

/// Grab state, owned by the hook thread.
struct Controller {
    hwnd: isize,
    hook: Option<HHOOK>,
    events: Sender<Event>,
}

/// What the hook procedure needs, owned by the hook thread (the procedure runs on it).
struct HookState {
    hwnd: isize,
    /// Keys whose press the hook swallowed, by [`keys::slot`] (for `repeat`, and so their
    /// release is swallowed too).
    down: [bool; keys::SLOTS],
    /// Keys Windows already had down when the hook went on, by virtual key. Their repeats and
    /// release pass through, or Windows would keep them stuck down after the grab.
    preheld: [bool; 256],
    events: Sender<Event>,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

fn is_foreground(hwnd: isize) -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { GetForegroundWindow() }.0 as isize == hwnd
}

fn with_controller<T>(f: impl FnOnce(&mut Controller) -> T) -> Option<T> {
    CONTROLLER
        .try_with(|cell| {
            let mut guard = cell.try_borrow_mut().ok()?;
            guard.as_mut().map(f)
        })
        .ok()
        .flatten()
}

fn hook_thread(
    hwnd: isize,
    events: Sender<Event>,
    commands: Receiver<Command>,
    id_tx: SyncSender<u32>,
) {
    let mut msg = MSG::default();
    // SAFETY: plain FFI calls on this thread with valid arguments.
    unsafe {
        // Create this thread's message queue before anyone posts to it.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        // While the grab is on, every key on the system waits for the hook procedure.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    CONTROLLER.with_borrow_mut(|c| {
        *c = Some(Controller {
            hwnd,
            hook: None,
            events,
        })
    });
    // Foreground changes and desktop switches, delivered through this thread's message loop.
    let win_events: Vec<HWINEVENTHOOK> = [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_DESKTOPSWITCH]
        .into_iter()
        .filter_map(|event| {
            // SAFETY: out-of-context hook with a valid callback; no module needed.
            let h = unsafe {
                SetWinEventHook(
                    event,
                    event,
                    None,
                    Some(on_win_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if h.is_invalid() {
                log::warn!("keyboard grab: SetWinEventHook({event:#x}) failed");
                None
            } else {
                Some(h)
            }
        })
        .collect();
    // SAFETY: no preconditions.
    let _ = id_tx.send(unsafe { GetCurrentThreadId() });

    'run: loop {
        // SAFETY: `msg` is a valid, writable MSG.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // WM_QUIT or error
        }
        if msg.message != WM_RD_COMMAND {
            // SAFETY: a message just retrieved by GetMessageW.
            unsafe { DispatchMessageW(&msg) };
            continue;
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::SetGrab { active, reply } => {
                    let result = with_controller(|c| c.set_grab(active))
                        .unwrap_or_else(|| Err("keyboard grab: busy".to_string()));
                    let _ = reply.send(result);
                }
                Command::Recheck => {
                    with_controller(|c| {
                        if !is_foreground(c.hwnd) {
                            c.remove(Why::Foreground);
                        }
                    });
                }
                Command::Navigation => {
                    with_controller(|c| c.remove(Why::Navigation));
                }
                Command::Stop => break 'run,
            }
        }
    }
    with_controller(|c| c.remove(Why::Stop));
    for h in win_events {
        // SAFETY: a hook this thread installed.
        let _ = unsafe { UnhookWinEvent(h) };
    }
}

impl Controller {
    fn set_grab(&mut self, active: bool) -> Result<(), String> {
        if !active {
            self.remove(Why::Page);
            return Ok(());
        }
        if self.hook.is_some() {
            return Ok(());
        }
        if !is_foreground(self.hwnd) {
            return Err("the rd window is not the foreground window".to_string());
        }
        self.install()
    }

    fn install(&mut self) -> Result<(), String> {
        let mut preheld = [false; 256];
        // 0x01..=0x06 are mouse buttons.
        for (vk, held) in preheld.iter_mut().enumerate().skip(0x08) {
            // SAFETY: plain FFI call. High bit set = down.
            *held = unsafe { GetAsyncKeyState(vk as i32) } < 0;
        }
        HOOK.with_borrow_mut(|h| {
            *h = Some(HookState {
                hwnd: self.hwnd,
                down: [false; keys::SLOTS],
                preheld,
                events: self.events.clone(),
            })
        });
        // SAFETY: the hook procedure lives in this executable; LL hooks run on this thread.
        let installed = unsafe {
            let module = GetModuleHandleW(PCWSTR::null())
                .ok()
                .map(|m| HINSTANCE(m.0));
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), module, 0)
        };
        match installed {
            Ok(h) => {
                self.hook = Some(h);
                log::debug!("keyboard grab on");
                Ok(())
            }
            Err(e) => {
                HOOK.with_borrow_mut(|h| *h = None);
                log::warn!("keyboard grab: SetWindowsHookExW failed: {e}");
                Err(format!("cannot install the keyboard hook: {e}"))
            }
        }
    }

    /// Remove the hook if it is installed. Unless the page asked for it, tell the page.
    fn remove(&mut self, why: Why) {
        let Some(h) = self.hook.take() else {
            return;
        };
        // SAFETY: a hook this thread installed.
        if let Err(e) = unsafe { UnhookWindowsHookEx(h) } {
            log::warn!("keyboard grab: UnhookWindowsHookEx failed: {e}");
        }
        HOOK.with_borrow_mut(|h| *h = None);
        log::debug!("keyboard grab off ({why:?})");
        if why != Why::Page {
            let _ = self.events.send(Event::Release);
        }
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    with_controller(|c| {
        if event == EVENT_SYSTEM_DESKTOPSWITCH {
            // Keys pressed or released on the other desktop never reach the hook.
            c.remove(Why::DesktopSwitch);
        } else if !is_foreground(c.hwnd) {
            c.remove(Why::Foreground);
        }
    });
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lParam points to a KBDLLHOOKSTRUCT.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if swallow(kb) {
            return LRESULT(1);
        }
    }
    // SAFETY: passing this hook call's own arguments on.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Decide about one key event; `true` swallows it. Must stay fast and must not panic.
fn swallow(kb: &KBDLLHOOKSTRUCT) -> bool {
    // Synthesized input (SendInput from any app) passes through untouched.
    if kb.flags.contains(LLKHF_INJECTED) {
        return false;
    }
    HOOK.try_with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(|state| state.on_key(kb)))
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

impl HookState {
    fn on_key(&mut self, kb: &KBDLLHOOKSTRUCT) -> bool {
        // The hook comes off when the window loses the foreground, but that notification is
        // asynchronous.
        if !is_foreground(self.hwnd) {
            return false;
        }
        let up = kb.flags.contains(LLKHF_UP);
        let sc = scancode_of(kb);
        if let Some(held) = self.preheld.get_mut(kb.vkCode as usize)
            && *held
        {
            // Pressed before the hook: Windows saw the press, so it gets the release. The page
            // saw the DOM keydown, so it gets the release as well.
            if up {
                *held = false;
                self.send(sc, false, false);
            }
            return false;
        }
        // Not a physical key of its own (fake shift, AltGr's fake LCtrl): swallow quietly.
        let Some(sc) = sc else {
            return true;
        };
        let Some(down) = self.down.get_mut(keys::slot(sc)) else {
            return true;
        };
        let was_down = std::mem::replace(down, !up);
        if up {
            self.send(Some(sc), false, false);
            // A release whose press the hook never swallowed goes to Windows as well.
            was_down
        } else {
            self.send(Some(sc), true, was_down);
            true
        }
    }

    fn send(&self, sc: Option<u16>, down: bool, repeat: bool) {
        if let Some(code) = sc.and_then(keys::code) {
            let _ = self.events.send(Event::Key { code, down, repeat });
        }
    }
}

fn scancode_of(kb: &KBDLLHOOKSTRUCT) -> Option<u16> {
    let (raw, extended) = if kb.scanCode != 0 {
        (kb.scanCode, kb.flags.contains(LLKHF_EXTENDED))
    } else {
        // Some keyboards and drivers send only a virtual key.
        // SAFETY: plain FFI call (reads the active layout's tables).
        let sc = unsafe { MapVirtualKeyW(kb.vkCode, MAPVK_VK_TO_VSC_EX) };
        (sc & 0xFF, sc & 0xFF00 == 0xE000)
    };
    keys::scancode(raw, extended, kb.vkCode)
}
