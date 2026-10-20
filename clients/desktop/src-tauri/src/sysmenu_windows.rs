//! Windows: "About rd Desktop" and "Check for Updates…" in the main window's title-bar menu
//! (right-click the title bar, or Alt+Space). The app has no menu bar on Windows.
//!
//! The items go into the window's system menu; a window subclass sees their `WM_SYSCOMMAND`
//! and passes every other message on.

use std::sync::OnceLock;

use tauri::{AppHandle, Manager, WebviewWindow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, GetSystemMenu, MF_SEPARATOR, MF_STRING, WM_SYSCOMMAND,
};
use windows::core::{PCWSTR, w};

// System menu command ids: below 0xF000 (the system's own) with the low four bits clear, since
// Windows uses those bits itself and the handler masks them off.
const ID_ABOUT: usize = 0x0A10;
const ID_CHECK: usize = 0x0A20;
/// Identifies this subclass on the window (any value unique per window and procedure).
const SUBCLASS_ID: usize = 0x7264;

static APP: OnceLock<AppHandle> = OnceLock::new();

/// Adds the items. Call on the main thread, which owns the window.
pub fn install(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window
        .hwnd()
        .map_err(|e| format!("no window handle: {e}"))?;
    let _ = APP.set(window.app_handle().clone());
    // SAFETY: plain FFI calls on the window's own thread with a valid window handle; the
    // subclass procedure stays valid for the life of the process.
    unsafe {
        let menu = GetSystemMenu(hwnd, false);
        if menu.is_invalid() {
            return Err("the window has no title-bar menu".into());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()).map_err(|e| e.to_string())?;
        AppendMenuW(menu, MF_STRING, ID_ABOUT, w!("About rd Desktop"))
            .map_err(|e| e.to_string())?;
        AppendMenuW(menu, MF_STRING, ID_CHECK, w!("Check for Updates…"))
            .map_err(|e| e.to_string())?;
        if !SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0).as_bool() {
            return Err("SetWindowSubclass failed".into());
        }
    }
    Ok(())
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_SYSCOMMAND {
        let command = wparam.0 & 0xFFF0;
        if (command == ID_ABOUT || command == ID_CHECK)
            && let Some(app) = APP.get()
        {
            // Not from inside the window procedure: the handlers open a window.
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if command == ID_ABOUT {
                    crate::update::about(&app);
                } else {
                    crate::update::check_now(&app);
                }
            });
            return LRESULT(0);
        }
    }
    // SAFETY: passing this message on to the next procedure in the chain.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
