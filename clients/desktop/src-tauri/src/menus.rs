//! The tray icon (macOS: menu bar) while the app runs, and "Check for Updates…" in the macOS
//! app menu. Windows' title-bar menu is in `sysmenu_windows.rs`.
//!
//! Menu events arrive on the main thread; the update window is built from a task (see
//! `update::open_window`), so these handlers never create windows themselves.

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

use crate::update;

const UPDATE: &str = "rd-update";
const AUTO: &str = "rd-auto-update";
const SHOW: &str = "rd-show";
const QUIT: &str = "rd-quit";
/// The macOS app menu's "Check for Updates…".
#[cfg(target_os = "macos")]
const CHECK: &str = "rd-check";

/// Tray items that change while the app runs.
struct Items {
    update: MenuItem<Wry>,
    auto: CheckMenuItem<Wry>,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let updates = app.state::<update::Updates>();
    let label = format!("rd Desktop {}", updates.version());
    let version = MenuItem::with_id(app, "rd-version", label, false, None::<&str>)?;
    let check = MenuItem::with_id(app, UPDATE, "Check for Updates…", true, None::<&str>)?;
    let (checked, enabled) = updates.auto_item();
    let auto = CheckMenuItem::with_id(
        app,
        AUTO,
        "Check for updates automatically",
        enabled,
        checked,
        None::<&str>,
    )?;
    let show = MenuItem::with_id(app, SHOW, "Show rd", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &version,
            &PredefinedMenuItem::separator(app)?,
            &check,
            &auto,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &quit,
        ],
    )?;
    let mut tray = TrayIconBuilder::with_id("rd")
        .tooltip("rd Desktop")
        .menu(&menu)
        .show_menu_on_left_click(true);
    // macOS: a template image, which the menu bar draws in its own colour. Windows: the app icon.
    if cfg!(target_os = "macos") {
        tray = tray
            .icon(tauri::include_image!("icons/tray-template.png"))
            .icon_as_template(true);
    } else if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    app.manage(Items {
        update: check,
        auto,
    });
    #[cfg(target_os = "macos")]
    app_menu(app)?;
    Ok(())
}

/// Tauri's default macOS menu (About, Hide, Quit, the Edit menu that copy and paste need, View,
/// Window), with "Check for Updates…" after About.
#[cfg(target_os = "macos")]
fn app_menu(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::MenuItemKind;
    let menu = Menu::default(app)?;
    if let Some(MenuItemKind::Submenu(first)) = menu.items()?.into_iter().next() {
        let check = MenuItem::with_id(app, CHECK, "Check for Updates…", true, None::<&str>)?;
        first.insert(&check, 1)?;
    }
    app.set_menu(menu)?;
    Ok(())
}

/// The tray's update item: "Check for Updates…", or what is waiting.
pub fn set_update_text(app: &AppHandle, text: &str) {
    if let Some(items) = app.try_state::<Items>()
        && let Err(e) = items.update.set_text(text)
    {
        log::warn!("cannot change the tray menu: {e}");
    }
}

/// Every menu's clicks (tray and app menu).
pub fn on_event(app: &AppHandle, id: &str) {
    match id {
        UPDATE => update::check_now(app),
        #[cfg(target_os = "macos")]
        CHECK => update::check_now(app),
        AUTO => {
            // The item has toggled itself already.
            if let Some(items) = app.try_state::<Items>() {
                match items.auto.is_checked() {
                    Ok(on) => update::set_auto(app, on),
                    Err(e) => log::warn!("tray menu: {e}"),
                }
            }
        }
        SHOW => show_main(app),
        QUIT => app.exit(0),
        _ => {}
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(crate::MAIN) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}
