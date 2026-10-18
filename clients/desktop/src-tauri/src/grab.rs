//! Grab requests from the page (`rd_set_grab`). On Windows they drive the low-level keyboard
//! hook (`grab_windows.rs`), on macOS an event tap (`grab_macos.rs`); elsewhere a grab is
//! refused, so the page keeps using DOM keys.
//!
//! `rd_set_grab({ active: true })` resolves only once the hook is installed; the page switches
//! from DOM key events to `rdNative.key` only then. The app never re-installs the hook on its
//! own: after it stops grabbing for its own reasons (focus lost, desktop switch, navigation,
//! closing) it calls `rdNative.release()` and waits for the page to ask again.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use tauri::WebviewWindow;

#[cfg(windows)]
type Platform = crate::grab_windows::HookThread;
#[cfg(target_os = "macos")]
type Platform = crate::grab_macos::TapGrab;
#[cfg(not(any(windows, target_os = "macos")))]
type Platform = ();

/// Hook/tap thread -> forwarder thread.
pub enum Event {
    Key {
        code: &'static str,
        down: bool,
        repeat: bool,
    },
    Release,
}

/// Starts the thread that runs the page calls on the webview, in order.
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

#[derive(Default)]
pub struct Grab {
    /// Last value the page sent (only to log transitions, not every repeat).
    last: Mutex<Option<bool>>,
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
    pub async fn set_active(&self, active: bool) -> Result<(), String> {
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
        #[cfg(any(windows, target_os = "macos"))]
        let result = match self.hook.get() {
            Some(hook) => hook.set_page_active(active).await,
            None if active => Err("keyboard grab unavailable".to_string()),
            None => Ok(()),
        };
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
