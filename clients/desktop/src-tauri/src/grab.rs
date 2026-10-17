//! Grab requests from the page (`rd_set_grab`). On Windows they drive the low-level keyboard
//! hook (see `grab_windows.rs`); elsewhere a grab is refused, so the page keeps using DOM keys.
//!
//! `rd_set_grab({ active: true })` resolves only once the hook is installed; the page switches
//! from DOM key events to `rdNative.key` only then. The app never re-installs the hook on its
//! own: after it stops grabbing for its own reasons (foreground lost, desktop switch,
//! navigation, closing) it calls `rdNative.release()` and waits for the page to ask again.

use std::sync::{Mutex, OnceLock};

use tauri::WebviewWindow;

#[derive(Default)]
pub struct Grab {
    /// Last value the page sent (only to log transitions, not every repeat).
    last: Mutex<Option<bool>>,
    #[cfg(windows)]
    hook: OnceLock<crate::grab_windows::HookThread>,
    #[cfg(not(windows))]
    hook: OnceLock<()>,
}

impl Grab {
    /// Start the platform grab for the main window (once).
    pub fn attach(&self, window: &WebviewWindow) {
        #[cfg(windows)]
        match crate::grab_windows::HookThread::start(window) {
            Ok(hook) => {
                let _ = self.hook.set(hook);
            }
            Err(e) => log::error!("keyboard grab unavailable: {e}"),
        }
        #[cfg(not(windows))]
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
        #[cfg(windows)]
        let result = match self.hook.get() {
            Some(hook) => hook.set_page_active(active).await,
            None if active => Err("keyboard grab unavailable".to_string()),
            None => Ok(()),
        };
        #[cfg(not(windows))]
        let result = if active {
            log::debug!("keyboard grab is only implemented on Windows; refusing");
            Err("keyboard grab not supported on this platform".to_string())
        } else {
            Ok(())
        };
        if let Err(e) = &result {
            log::debug!("rd_set_grab active={active}: {e}");
        }
        result
    }

    /// A new page is loading in the main window: drop the grab (the old page's unload-time
    /// `active: false` may never arrive).
    pub fn page_changed(&self) {
        self.forget_last();
        #[cfg(windows)]
        if let Some(hook) = self.hook.get() {
            hook.navigation();
        }
    }

    /// The main window gained or lost focus. The hook thread also follows foreground changes
    /// itself; this is a second trigger.
    pub fn focus_changed(&self) {
        #[cfg(windows)]
        if let Some(hook) = self.hook.get() {
            hook.recheck_foreground();
        }
    }

    /// The window is closing: drop the grab (and tell the page) while the webview still exists.
    pub fn shutdown(&self) {
        #[cfg(windows)]
        if let Some(hook) = self.hook.get() {
            hook.stop();
        }
    }

    fn forget_last(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
