//! Grab requests from the page (`rd_set_grab`). On Windows they drive the low-level keyboard
//! hook (`grab_windows.rs`), on macOS an event tap (`grab_macos.rs`); elsewhere a grab is
//! refused, so the page keeps using DOM keys.
//!
//! `rd_set_grab({ active: true })` resolves only once the hook is installed; the page switches
//! from DOM key events to `rdNative.key` only then. The app never re-installs the hook on its
//! own: after it stops grabbing for its own reasons (focus lost, desktop switch, navigation,
//! closing) it calls `rdNative.release()` and waits for the page to ask again.

#[cfg(desktop)]
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::WebviewWindow;

#[cfg(windows)]
type Platform = crate::grab_windows::HookThread;
#[cfg(target_os = "macos")]
type Platform = crate::grab_macos::TapGrab;
#[cfg(not(any(windows, target_os = "macos")))]
type Platform = ();

/// What the viewer's Troubleshoot screen shows about the keyboard grab (`rd_keyboard_status`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardStatus {
    /// "macos", "windows" or "other".
    pub platform: &'static str,
    pub version: String,
    /// The app can grab the keyboard on this platform.
    pub grab: bool,
    /// macOS: the Accessibility permission (the tap needs it). `None` elsewhere.
    pub accessibility: Option<bool>,
    /// macOS: Secure Keyboard Entry is on, so macOS hides keys from the tap. `None` elsewhere.
    pub secure_input: Option<bool>,
}

pub fn keyboard_status(version: String) -> KeyboardStatus {
    #[cfg(target_os = "macos")]
    {
        let (accessibility, secure_input) = crate::grab_macos::permission_status();
        KeyboardStatus {
            platform: "macos",
            version,
            grab: true,
            accessibility: Some(accessibility),
            secure_input: Some(secure_input),
        }
    }
    #[cfg(not(target_os = "macos"))]
    KeyboardStatus {
        platform: if cfg!(windows) { "windows" } else { "other" },
        version,
        grab: cfg!(windows),
        accessibility: None,
        secure_input: None,
    }
}

/// Opens the system settings page that the grab depends on (macOS: Accessibility).
pub fn open_keyboard_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return crate::grab_macos::open_accessibility_settings();
    #[cfg(not(target_os = "macos"))]
    Err("no keyboard settings to open on this platform".into())
}

/// Hook/tap thread -> forwarder thread.
#[cfg(desktop)]
pub enum Event {
    Key {
        code: &'static str,
        down: bool,
        repeat: bool,
    },
    Release,
}

/// Starts the thread that runs the page calls on the webview, in order.
#[cfg(desktop)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub fn spawn_forwarder(window: &WebviewWindow) -> Result<Sender<Event>, String> {
    let (tx, rx) = mpsc::channel();
    let target = window.clone();
    std::thread::Builder::new()
        .name("rd-kbd-forward".into())
        .spawn(move || forward(target, rx))
        .map_err(|e| e.to_string())?;
    Ok(tx)
}

/// Never logs the script (it names keys).
#[cfg(desktop)]
fn forward(window: WebviewWindow, rx: Receiver<Event>) {
    let mut chord = FullscreenChord::default();
    for event in rx {
        let js = match event {
            Event::Key { code, down, repeat } => {
                match chord.key(code, down, repeat) {
                    ChordKey::Forward => {}
                    ChordKey::Swallow => continue,
                    ChordKey::Toggle => {
                        toggle_fullscreen(&window);
                        continue;
                    }
                }
                // Codes come from the static key table; JSON-escape them anyway.
                let Ok(code) = serde_json::to_string(code) else {
                    continue;
                };
                format!("window.rdNative&&window.rdNative.key({code},{down},{repeat})")
            }
            Event::Release => {
                chord = FullscreenChord::default();
                "window.rdNative&&window.rdNative.release()".to_string()
            }
        };
        if let Err(e) = window.eval(js) {
            log::warn!("keyboard grab: cannot reach the page: {e}");
        }
    }
}

/// Ctrl+Alt+Shift+F during a grab. The page would toggle fullscreen itself, but a key that
/// arrives through `rdNative.key` carries no user gesture, so WebView2 refuses
/// `requestFullscreen` and nothing happens. The app toggles the window instead and keeps the
/// F press from the page (the page swallows its chords too, so the remote side sees the same).
#[cfg(desktop)]
#[derive(Default)]
struct FullscreenChord {
    /// Held modifiers, one bit per physical key (see `modifier_bit`).
    mods: u8,
    /// The F press that toggled is still down: its autorepeat and release are swallowed too.
    swallowing: bool,
}

#[cfg(desktop)]
#[derive(Debug, PartialEq)]
enum ChordKey {
    Forward,
    Swallow,
    Toggle,
}

#[cfg(desktop)]
const CTRL: u8 = 0b0000_0011;
#[cfg(desktop)]
const ALT: u8 = 0b0000_1100;
#[cfg(desktop)]
const SHIFT: u8 = 0b0011_0000;

#[cfg(desktop)]
fn modifier_bit(code: &str) -> u8 {
    match code {
        "ControlLeft" => 0b0000_0001,
        "ControlRight" => 0b0000_0010,
        "AltLeft" => 0b0000_0100,
        "AltRight" => 0b0000_1000,
        "ShiftLeft" => 0b0001_0000,
        "ShiftRight" => 0b0010_0000,
        _ => 0,
    }
}

#[cfg(desktop)]
impl FullscreenChord {
    fn key(&mut self, code: &str, down: bool, repeat: bool) -> ChordKey {
        let bit = modifier_bit(code);
        if bit != 0 {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
            return ChordKey::Forward;
        }
        if code != "KeyF" {
            return ChordKey::Forward;
        }
        if !down {
            return if std::mem::take(&mut self.swallowing) {
                ChordKey::Swallow
            } else {
                ChordKey::Forward
            };
        }
        if self.swallowing {
            return ChordKey::Swallow;
        }
        // The Windows hook already drops AltGr's fake Ctrl, so a held Ctrl here is a real one.
        if self.mods & CTRL != 0 && self.mods & ALT != 0 && self.mods & SHIFT != 0 {
            self.swallowing = true;
            return if repeat {
                ChordKey::Swallow
            } else {
                ChordKey::Toggle
            };
        }
        ChordKey::Forward
    }
}

#[cfg(desktop)]
fn toggle_fullscreen(window: &WebviewWindow) {
    let on = !window.is_fullscreen().unwrap_or(false);
    if !on {
        // Entered from the page (its Fullscreen button): leave the page's fullscreen as well.
        let _ = window.eval("document.fullscreenElement&&document.exitFullscreen()");
    }
    match window.set_fullscreen(on) {
        Ok(()) => log::info!(
            "fullscreen {} (Ctrl+Alt+Shift+F)",
            if on { "on" } else { "off" }
        ),
        Err(e) => log::warn!("cannot change fullscreen: {e}"),
    }
}

/// Applies `rd_set_grab` calls in the page's order. Tauri runs each call as its own task, so a
/// quick `true`, `false`, `true` from the page can otherwise reach the hook in any order and leave
/// it off while the page believes it is on.
#[derive(Default)]
pub struct Order {
    last: Mutex<u64>,
}

impl Order {
    /// Runs `apply` unless a newer call was already applied; holds the lock meanwhile, so calls
    /// are applied (on Windows: queued to the hook thread) in sequence order. Calls without a
    /// number (older pages, the selftest) are always applied. A repeated number is applied
    /// again: that is the latest state, and applying it twice is harmless.
    #[cfg(any(windows, target_os = "macos"))]
    pub fn admit<T>(
        &self,
        seq: Option<u64>,
        apply: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seq) = seq {
            if seq < *last {
                return Err(format!(
                    "superseded (call {seq}, already applied {})",
                    *last
                ));
            }
            *last = seq;
        }
        apply()
    }

    /// A new page numbers its calls from the start again.
    pub fn reset(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = 0;
    }
}

#[derive(Default)]
pub struct Grab {
    /// Last value the page sent (only to log transitions, not every repeat).
    last: Mutex<Option<bool>>,
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    order: std::sync::Arc<Order>,
    hook: OnceLock<Platform>,
}

impl Grab {
    /// Start the platform grab for the main window (once).
    pub fn attach(&self, window: &WebviewWindow) {
        #[cfg(any(windows, target_os = "macos"))]
        match Platform::start(window) {
            Ok(hook) => {
                let _ = self.hook.set(hook);
            }
            Err(e) => log::error!("keyboard grab unavailable: {e}"),
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = window;
            let _ = self.hook.set(());
        }
    }

    /// The page's `rd_set_grab({ active })`. `Ok` for `true` means the keys now come through
    /// `rdNative.key`; an error means they do not (the page keeps its DOM listeners).
    pub async fn set_active(&self, active: bool, seq: Option<u64>) -> Result<(), String> {
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if *last != Some(active) {
                if cfg!(debug_assertions) {
                    log::info!("rd_set_grab active={active}");
                } else {
                    log::debug!("rd_set_grab active={active}");
                }
            }
            *last = Some(active);
        }
        #[cfg(windows)]
        let result = match self.hook.get() {
            Some(hook) => match self.order.admit(seq, || hook.request(active)) {
                Ok(answer) => crate::grab_windows::HookThread::wait(answer).await,
                Err(e) => Err(e),
            },
            None if active => Err("keyboard grab unavailable".to_string()),
            None => Ok(()),
        };
        #[cfg(target_os = "macos")]
        let result = match self.hook.get() {
            Some(hook) => hook.set_page_active(active, seq, self.order.clone()).await,
            None if active => Err("keyboard grab unavailable".to_string()),
            None => Ok(()),
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = seq;
        #[cfg(not(any(windows, target_os = "macos")))]
        let result = if active {
            log::debug!("keyboard grab is only implemented on Windows and macOS; refusing");
            Err("keyboard grab not supported on this platform".to_string())
        } else {
            Ok(())
        };
        if let Err(e) = &result {
            if active {
                log::info!("keyboard grab refused: {e}");
            } else {
                log::debug!("rd_set_grab active={active}: {e}");
            }
        }
        result
    }

    /// A new page is loading in the main window: drop the grab (the old page's unload-time
    /// `active: false` may never arrive).
    pub fn page_changed(&self) {
        self.forget_last();
        self.order.reset();
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(hook) = self.hook.get() {
            hook.navigation();
        }
    }

    /// The main window gained or lost focus.
    pub fn focus_changed(&self, focused: bool) {
        // The Windows hook thread follows foreground changes itself; this is a second trigger.
        #[cfg(windows)]
        if let Some(hook) = self.hook.get() {
            let _ = focused;
            hook.recheck_foreground();
        }
        #[cfg(target_os = "macos")]
        if let Some(hook) = self.hook.get() {
            hook.focus_changed(focused);
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = focused;
    }

    /// The window is closing: drop the grab (and tell the page) while the webview still exists.
    pub fn shutdown(&self) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(hook) = self.hook.get() {
            hook.stop();
        }
    }

    fn forget_last(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(all(test, desktop))]
mod tests {
    use super::{ChordKey, FullscreenChord};

    fn keys(chord: &mut FullscreenChord, seq: &[(&str, bool, bool)]) -> Vec<ChordKey> {
        seq.iter().map(|&(c, d, r)| chord.key(c, d, r)).collect()
    }

    #[test]
    fn ctrl_alt_shift_f_toggles_and_hides_the_f() {
        let mut c = FullscreenChord::default();
        let got = keys(
            &mut c,
            &[
                ("ControlLeft", true, false),
                ("AltLeft", true, false),
                ("ShiftRight", true, false),
                ("KeyF", true, false),
                ("KeyF", true, true),
                ("KeyF", false, false),
                ("ShiftRight", false, false),
            ],
        );
        use ChordKey::*;
        assert_eq!(
            got,
            [Forward, Forward, Forward, Toggle, Swallow, Swallow, Forward]
        );
        // Plain F afterwards types again.
        assert_eq!(
            keys(&mut c, &[("KeyF", true, false), ("KeyF", false, false)]),
            [Forward, Forward]
        );
    }

    #[test]
    fn f_without_the_full_chord_types() {
        let mut c = FullscreenChord::default();
        for mods in [
            &["ControlLeft", "AltRight"][..],
            &["AltLeft", "ShiftLeft"],
            &["ControlRight", "ShiftLeft"],
        ] {
            for m in mods {
                assert_eq!(c.key(m, true, false), ChordKey::Forward);
            }
            assert_eq!(c.key("KeyF", true, false), ChordKey::Forward);
            assert_eq!(c.key("KeyF", false, false), ChordKey::Forward);
            for m in mods {
                assert_eq!(c.key(m, false, false), ChordKey::Forward);
            }
        }
    }

    #[test]
    fn other_chord_keys_still_reach_the_page() {
        let mut c = FullscreenChord::default();
        for m in ["ControlLeft", "AltLeft", "ShiftLeft"] {
            c.key(m, true, false);
        }
        assert_eq!(c.key("KeyD", true, false), ChordKey::Forward);
    }
}
