//! macOS keyboard grab.
//!
//! While the page asks for the grab (`rd_set_grab({ active: true })`) and the rd window is the
//! key window, an event tap at the HID level (ahead of the Dock, Spotlight and the system
//! shortcuts) swallows every key and forwards it to the page as
//! `window.rdNative.key(code, down, repeat)`, so Cmd+Tab, Cmd+Space, Ctrl+arrows, Cmd+Q, … go
//! to the remote machine.
//!
//! The tap needs the Accessibility permission. Without it the grab is refused (the page keeps
//! its DOM keys) and macOS is asked once per launch to show its permission prompt.
//!
//! The tap runs on its own thread and run loop, created per grab. It comes off when the page
//! says so, or for the app's own reasons: the window stopped being the key window, a new page
//! loads, the window closes, or macOS disabled the tap. Then the page gets
//! `window.rdNative.release()` and the app waits for the page to ask again.
//!
//! Not grabbed: Caps Lock (macOS toggles it before any tap; see `keys_mac`), Ctrl+Cmd+Q and
//! Cmd+Option+Esc (always local), media/brightness keys (not key events), and synthesized
//! input from other apps. The callback only does table lookups and a channel send; keys are
//! never logged.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::WebviewWindow;

use crate::grab::Event;
use crate::keys_mac::{Input, KEYCODES, Kind, Tracker};

// ---------------------------------------------------------------------------------------------
// FFI

type CFTypeRef = *const c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type TapCallback =
    unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

/// `kCGHIDEventTap`: where HID events enter the window server, before any system shortcut.
const HID_EVENT_TAP: u32 = 0;
/// `kCGSessionEventTap`: where they enter the login session (still ahead of the Dock's Cmd+Tab).
const SESSION_EVENT_TAP: u32 = 1;
const HEAD_INSERT_EVENT_TAP: u32 = 0;
/// `kCGEventTapOptionDefault`: an active tap (may drop events).
const TAP_OPTION_DEFAULT: u32 = 0;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
const FIELD_AUTOREPEAT: u32 = 8;
const FIELD_KEYCODE: u32 = 9;
const FIELD_SOURCE_PID: u32 = 41;
/// `kCGEventSourceStateHIDSystemState`.
const HID_SYSTEM_STATE: i32 = 1;
/// `QOS_CLASS_USER_INTERACTIVE`: while grabbing, every key on the system waits for the callback.
const QOS_USER_INTERACTIVE: u32 = 0x21;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_len: usize,
        actual_len: *mut usize,
        buf: *mut u16,
    );
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFRunLoopDefaultMode: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFMachPortCreateRunLoopSource(
        alloc: CFTypeRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopStop(rl: CFRunLoopRef);
    fn CFRunLoopWakeUp(rl: CFRunLoopRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
}

/// A retained `CFRunLoopRef`. `CFRunLoopStop` and `CFRunLoopWakeUp` may be called from any
/// thread.
struct RunLoop(CFRunLoopRef);

// SAFETY: only used for the thread-safe CFRunLoopStop/WakeUp, and released once.
unsafe impl Send for RunLoop {}

impl RunLoop {
    fn current() -> Self {
        // SAFETY: plain CF calls; the retain is balanced in Drop.
        unsafe { Self(CFRetain(CFRunLoopGetCurrent() as CFTypeRef) as CFRunLoopRef) }
    }

    fn stop(&self) {
        // SAFETY: a retained, valid run loop.
        unsafe {
            CFRunLoopStop(self.0);
            CFRunLoopWakeUp(self.0);
        }
    }
}

impl Drop for RunLoop {
    fn drop(&mut self) {
        // SAFETY: balances the retain in `current`.
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}

fn accessibility_trusted() -> bool {
    // SAFETY: no arguments.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Shows macOS's "rd Desktop would like to control this computer using accessibility features"
/// prompt (with a button to the right System Settings pane), and adds the app to the list.
fn prompt_for_accessibility() {
    // SAFETY: a one-entry CFDictionary with the documented key and kCFBooleanTrue, released
    // after use.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt as CFTypeRef];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        if options.is_null() {
            return;
        }
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
    }
}

/// For the viewer's Troubleshoot screen: (Accessibility granted, Secure Keyboard Entry on).
pub fn permission_status() -> (bool, bool) {
    (accessibility_trusted(), secure_input_enabled())
}

/// System Settings › Privacy & Security › Accessibility.
pub fn open_accessibility_settings() -> Result<(), String> {
    open::that_detached(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    )
    .map_err(|e| format!("cannot open System Settings: {e}"))
}

fn secure_input_enabled() -> bool {
    // SAFETY: no arguments.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

fn keys_down_now() -> [bool; KEYCODES] {
    let mut held = [false; KEYCODES];
    for (keycode, down) in held.iter_mut().enumerate() {
        // SAFETY: plain query of the HID system's key state.
        *down = unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, keycode as u16) };
    }
    held
}

// ---------------------------------------------------------------------------------------------
// grab

/// Why the tap comes off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Page,
    Focus,
    Navigation,
    Stop,
}

pub struct TapGrab {
    inner: Arc<Inner>,
}

struct Inner {
    events: Sender<Event>,
    /// The rd window is the key window (from Tauri's focus events).
    focused: Arc<AtomicBool>,
    running: Mutex<Option<Running>>,
    /// The Accessibility prompt was shown in this launch.
    prompted: AtomicBool,
    /// The Input Monitoring prompt was shown in this launch.
    asked_listen: Arc<AtomicBool>,
    /// Debug builds, `RD_DESKTOP_GRAB_INJECTED=1`: grab synthesized keys too (for testing with
    /// keys posted by another process).
    grab_injected: bool,
}

/// One installed tap and its thread.
struct Running {
    run_loop: RunLoop,
    thread: JoinHandle<()>,
    shared: Arc<TapShared>,
}

/// What the tap callback reads, owned by an `Arc` that outlives the tap.
struct TapShared {
    focused: Arc<AtomicBool>,
    events: Sender<Event>,
    tracker: Mutex<Tracker>,
    /// Ask the tap thread to finish.
    stop: AtomicBool,
    /// macOS disabled the tap; the page has been sent `release()` already.
    disabled: AtomicBool,
    grab_injected: bool,
    asked_listen: Arc<AtomicBool>,
    /// Counts for the log line when the grab ends (never which keys).
    forwarded: AtomicU32,
    injected: AtomicU32,
}

impl TapGrab {
    pub fn start(window: &WebviewWindow) -> Result<Self, String> {
        let events = crate::grab::spawn_forwarder(window)?;
        let focused = window.is_focused().unwrap_or(false);
        Ok(Self {
            inner: Arc::new(Inner {
                events,
                focused: Arc::new(AtomicBool::new(focused)),
                running: Mutex::new(None),
                prompted: AtomicBool::new(false),
                asked_listen: Arc::default(),
                grab_injected: cfg!(debug_assertions)
                    && std::env::var_os("RD_DESKTOP_GRAB_INJECTED").is_some(),
            }),
        })
    }

    /// Resolves once the tap is installed (`true`) or removed (`false`). Calls are applied in
    /// `seq` order (see [`crate::grab::Order`]).
    pub async fn set_page_active(
        &self,
        active: bool,
        seq: Option<u64>,
        order: std::sync::Arc<crate::grab::Order>,
    ) -> Result<(), String> {
        let inner = self.inner.clone();
        tauri::async_runtime::spawn_blocking(move || {
            order.admit(seq, || {
                if active {
                    inner.install()
                } else {
                    inner.remove(Why::Page);
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// The rd window became or stopped being the key window.
    pub fn focus_changed(&self, focused: bool) {
        self.inner.focused.store(focused, Ordering::SeqCst);
        if !focused {
            self.inner.remove(Why::Focus);
        }
    }

    pub fn navigation(&self) {
        self.inner.remove(Why::Navigation);
    }

    pub fn stop(&self) {
        self.inner.remove(Why::Stop);
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn install(&self) -> Result<(), String> {
        let mut running = self.lock();
        if let Some(r) = running.as_ref() {
            if !r.shared.disabled.load(Ordering::SeqCst) {
                return Ok(());
            }
            // macOS disabled that tap; replace it.
            if let Some(r) = running.take() {
                r.finish();
            }
        }
        if !self.focused.load(Ordering::SeqCst) {
            return Err("the rd window is not the active window".into());
        }
        if !accessibility_trusted() {
            if !self.prompted.swap(true, Ordering::SeqCst) {
                log::info!("keyboard grab: asking for the Accessibility permission");
                prompt_for_accessibility();
            }
            return Err(
                "rd Desktop needs the Accessibility permission to grab the keyboard \
                 (System Settings > Privacy & Security > Accessibility)"
                    .into(),
            );
        }
        if secure_input_enabled() {
            return Err(
                "Secure Keyboard Entry is on (a password field or a terminal \
                 setting), so macOS hides keys from rd"
                    .into(),
            );
        }
        let shared = Arc::new(TapShared {
            focused: self.focused.clone(),
            events: self.events.clone(),
            tracker: Mutex::new(Tracker::new(keys_down_now())),
            stop: AtomicBool::new(false),
            disabled: AtomicBool::new(false),
            grab_injected: self.grab_injected,
            asked_listen: self.asked_listen.clone(),
            forwarded: AtomicU32::new(0),
            injected: AtomicU32::new(0),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rd-kbd-tap".into())
            .spawn(move || tap_thread(thread_shared, ready_tx))
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(run_loop)) => {
                log::info!("keyboard grab on");
                *running = Some(Running {
                    run_loop,
                    thread,
                    shared,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::SeqCst);
                Err("keyboard grab: the tap thread did not start".into())
            }
        }
    }

    /// Remove the tap if it is installed. Unless the page asked for it, tell the page.
    fn remove(&self, why: Why) {
        let Some(running) = self.lock().take() else {
            return;
        };
        let shared = running.shared.clone();
        let already_released = shared.disabled.load(Ordering::SeqCst);
        running.finish();
        let forwarded = shared.forwarded.load(Ordering::Relaxed);
        let injected = shared.injected.load(Ordering::Relaxed);
        if already_released {
            log::warn!("keyboard grab: macOS disabled the keyboard tap");
        }
        log::info!(
            "keyboard grab off ({why:?}): {forwarded} key events forwarded, \
             {injected} synthesized ones passed through"
        );
        if why != Why::Page && !already_released {
            let _ = self.events.send(Event::Release);
        }
    }
}

impl Running {
    fn finish(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.run_loop.stop();
        let _ = self.thread.join();
    }
}

fn tap_thread(shared: Arc<TapShared>, ready: mpsc::SyncSender<Result<RunLoop, String>>) {
    // SAFETY: affects only this thread.
    unsafe { pthread_set_qos_class_self_np(QOS_USER_INTERACTIVE, 0) };
    let mask = (1u64 << KEY_DOWN) | (1u64 << KEY_UP) | (1u64 << FLAGS_CHANGED);
    let user_info = Arc::as_ptr(&shared) as *mut c_void;
    let create = |location| {
        // SAFETY: `user_info` points to a TapShared that this thread keeps alive (its Arc)
        // until after the tap is invalidated below; the callback only runs on this thread's
        // run loop.
        unsafe {
            CGEventTapCreate(
                location,
                HEAD_INSERT_EVENT_TAP,
                TAP_OPTION_DEFAULT,
                mask,
                tap_callback,
                user_info,
            )
        }
    };
    let mut tap = create(HID_EVENT_TAP);
    if tap.is_null() {
        tap = create(SESSION_EVENT_TAP);
        if !tap.is_null() {
            log::info!("keyboard grab: using a session-level tap (HID level refused)");
        }
    }
    if tap.is_null() {
        // Some macOS versions also want Input Monitoring for keyboard taps.
        // SAFETY: plain permission queries; the request shows the system prompt once.
        let listen = unsafe { CGPreflightListenEventAccess() };
        if !listen && !shared.asked_listen.swap(true, Ordering::SeqCst) {
            unsafe { CGRequestListenEventAccess() };
        }
        let _ = ready.send(Err(if listen {
            "macOS refused the keyboard tap".to_string()
        } else {
            "macOS refused the keyboard tap: allow rd Desktop under System Settings > \
             Privacy & Security > Input Monitoring"
                .to_string()
        }));
        return;
    }
    // SAFETY: a valid tap; the source and the tap are released below.
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
    if source.is_null() {
        // SAFETY: the tap created above.
        unsafe {
            CFMachPortInvalidate(tap);
            CFRelease(tap as CFTypeRef);
        }
        let _ = ready.send(Err("keyboard grab: no run loop source for the tap".into()));
        return;
    }
    // SAFETY: valid run loop, source and mode.
    let rl = unsafe {
        let rl = CFRunLoopGetCurrent();
        CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        rl
    };
    if ready.send(Ok(RunLoop::current())).is_err() {
        shared.stop.store(true, Ordering::SeqCst);
    }
    while !shared.stop.load(Ordering::SeqCst) {
        // SAFETY: runs this thread's run loop; returns on CFRunLoopStop or after the timeout.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0) };
    }
    // SAFETY: tear down what was set up above, on the same thread.
    unsafe {
        CGEventTapEnable(tap, false);
        CFRunLoopRemoveSource(rl, source, kCFRunLoopCommonModes);
        CFMachPortInvalidate(tap);
        CFRelease(source as CFTypeRef);
        CFRelease(tap as CFTypeRef);
    }
    drop(shared);
}

/// The tap callback. Must stay fast and must not panic.
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    // SAFETY: the TapShared the tap was created with; alive while the tap exists.
    let shared = unsafe { &*(user_info as *const TapShared) };
    let kind = match event_type {
        KEY_DOWN => Kind::Down,
        KEY_UP => Kind::Up,
        FLAGS_CHANGED => Kind::FlagsChanged,
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => {
            // Keys may have gone to this Mac meanwhile: end the grab, the page releases all.
            if !shared.disabled.swap(true, Ordering::SeqCst) {
                let _ = shared.events.send(Event::Release);
            }
            shared.stop.store(true, Ordering::SeqCst);
            // SAFETY: this thread's own run loop.
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
            return event;
        }
        _ => return event,
    };
    if event.is_null()
        || shared.disabled.load(Ordering::Relaxed)
        || !shared.focused.load(Ordering::Relaxed)
    {
        return event;
    }
    // SAFETY: `event` is a valid keyboard event for the duration of the callback.
    let pid = unsafe { CGEventGetIntegerValueField(event, FIELD_SOURCE_PID) };
    if pid != 0 && !shared.grab_injected {
        // Synthesized by a process (an rd agent on this Mac, a remapping tool, …): pass it on,
        // or an agent here would feed its own keys back to itself.
        if shared.injected.fetch_add(1, Ordering::Relaxed) == 0 {
            log::info!("keyboard grab: passing synthesized keys from pid {pid}");
        }
        return event;
    }
    // SAFETY: as above.
    let input = unsafe {
        Input {
            kind,
            keycode: CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16,
            flags: CGEventGetFlags(event),
            autorepeat: CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) != 0,
            ch: if kind == Kind::Down {
                typed_char(event)
            } else {
                None
            },
        }
    };
    let Ok(mut tracker) = shared.tracker.try_lock() else {
        return event;
    };
    let verdict = tracker.on_event(&input);
    drop(tracker);
    if let Some(k) = verdict.forward {
        shared.forwarded.fetch_add(1, Ordering::Relaxed);
        let _ = shared.events.send(Event::Key {
            code: k.code,
            down: k.down,
            repeat: k.repeat,
        });
    }
    if verdict.swallow {
        std::ptr::null_mut()
    } else {
        event
    }
}

/// The (lowercase) character a key-down types, if it is a single one. With Ctrl held this can
/// be a control code (^Q = U+0011); `keys_mac` accounts for that.
///
/// # Safety
/// `event` must be a valid keyboard event.
unsafe fn typed_char(event: CGEventRef) -> Option<char> {
    let mut buf = [0u16; 4];
    let mut len = 0usize;
    // SAFETY: `buf` has room for `buf.len()` UTF-16 units.
    unsafe { CGEventKeyboardGetUnicodeString(event, buf.len(), &mut len, buf.as_mut_ptr()) };
    let mut chars = char::decode_utf16(buf[..len.min(buf.len())].iter().copied());
    let c = chars.next()?.ok()?;
    if chars.next().is_some() {
        return None;
    }
    c.to_lowercase().next()
}
