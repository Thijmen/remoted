//! rd Desktop: the rd web viewer (loaded from the gateway) in its own window. On Windows and
//! macOS it grabs the keys a browser tab cannot see (Alt+Tab, the Windows key, Cmd+Tab, …) while
//! a session has the keyboard, and updates itself from GitHub (`update.rs`). On Android the same
//! window is the whole app: the system WebView, the setup page and the gateway, with no keyboard
//! grab (a phone has no system shortcuts to take) and no in-app viewer copy.
//!
//! Bridge contract: docs/desktop-client.md; updates: docs/updates.md.

mod config;
mod grab;
#[cfg(target_os = "macos")]
mod grab_macos;
#[cfg(windows)]
mod grab_windows;
/// Moving the app out of the way for an update is a desktop concern (installers, app bundles).
#[cfg(desktop)]
mod installable;
mod keys;
mod keys_mac;
#[cfg(desktop)]
mod menus;
#[cfg(desktop)]
mod notes;
#[cfg(desktop)]
mod notice;
#[cfg(windows)]
mod sysmenu_windows;
#[cfg(desktop)]
mod update;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use tauri::ipc::CapabilityBuilder;
use tauri::webview::PageLoadEvent;
#[cfg(desktop)]
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_log::{RotationStrategy, Target, TargetKind};

/// Label of the one window (capabilities refer to it).
const MAIN: &str = "main";

/// The settings file, in the app config directory (Windows: %APPDATA%\dev.thijmen.rd.desktop,
/// macOS: ~/Library/Application Support/dev.thijmen.rd.desktop).
const SETTINGS_FILE: &str = "settings.json";

/// The gateway the window uses now (`None` until the setup page saves one), and the origins
/// already granted the gateway commands (`rd_set_grab`, `rd_whats_new`, `rd_keyboard_status`,
/// `rd_open_keyboard_settings`).
#[derive(Default)]
struct Current {
    settings: RwLock<Option<config::Settings>>,
    granted: Mutex<HashSet<String>>,
}

impl Current {
    fn settings(&self) -> Option<config::Settings> {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Makes `settings` current and lets its gateway origin (only) call `rd_set_grab`,
    /// `rd_keyboard_status` and `rd_open_keyboard_settings` (and, on the desktop, `rd_whats_new`,
    /// the notice after an update).
    fn apply<R: tauri::Runtime>(
        &self,
        app: &impl Manager<R>,
        settings: config::Settings,
    ) -> tauri::Result<()> {
        let origin = config::origin(&settings.gateway_url);
        let mut granted = self.granted.lock().unwrap_or_else(|e| e.into_inner());
        if !granted.contains(&origin) {
            let capability = CapabilityBuilder::new(format!("gateway-{}", granted.len()))
                .remote(origin.clone())
                .local(false)
                .window(MAIN)
                .permission("allow-rd-set-grab");
            // The update notice only exists on the desktop (see update.rs).
            #[cfg(desktop)]
            let capability = capability.permission("allow-rd-whats-new");
            app.add_capability(
                capability
                    .permission("allow-rd-keyboard-status")
                    .permission("allow-rd-open-keyboard-settings"),
            )?;
            log::info!(
                "{origin} may call rd_set_grab, rd_keyboard_status and \
                 rd_open_keyboard_settings"
            );
            granted.insert(origin);
        }
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = Some(settings);
        Ok(())
    }
}

/// Page -> app: does a session have the keyboard? (bridge contract v1). For `active: true`
/// it resolves only once keys come through `rdNative.key`; an error means they do not.
/// `seq` (optional, increasing per page) orders the calls, which Tauri runs concurrently.
#[tauri::command]
async fn rd_set_grab(
    active: bool,
    seq: Option<u64>,
    webview: tauri::Webview,
    grab: tauri::State<'_, grab::Grab>,
) -> Result<(), String> {
    if webview.label() != MAIN {
        return Err("not the main window".into());
    }
    grab.set_active(active, seq).await
}

/// Page -> app: can the app grab the keyboard here, and what stands in the way (the viewer's
/// Troubleshoot screen). Read-only.
#[tauri::command]
fn rd_keyboard_status(webview: tauri::Webview) -> Result<grab::KeyboardStatus, String> {
    if webview.label() != MAIN {
        return Err("not the main window".into());
    }
    let version = webview.app_handle().package_info().version.to_string();
    Ok(grab::keyboard_status(version))
}

/// Page -> app: open the system settings page the grab needs (macOS: Privacy & Security ›
/// Accessibility). Opens that one fixed page, nothing else.
#[tauri::command]
fn rd_open_keyboard_settings(webview: tauri::Webview) -> Result<(), String> {
    if webview.label() != MAIN {
        return Err("not the main window".into());
    }
    log::info!("opening the keyboard permission settings (Troubleshoot)");
    grab::open_keyboard_settings()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupInfo {
    gateway_url: Option<String>,
    sign_in_domain: Option<String>,
    settings_path: String,
}

/// Setup page -> app: what is configured now (to fill in the form).
#[tauri::command]
fn rd_setup_info(
    webview: tauri::Webview,
    current: tauri::State<'_, Current>,
) -> Result<SetupInfo, String> {
    if webview.label() != MAIN {
        return Err("not the main window".into());
    }
    let settings = current.settings();
    Ok(SetupInfo {
        gateway_url: settings.as_ref().map(|s| s.gateway_url.to_string()),
        sign_in_domain: settings.and_then(|s| s.sign_in_domain),
        settings_path: settings_path(webview.app_handle())
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    })
}

/// Setup page -> app: save the gateway and open it.
#[tauri::command]
fn rd_save_gateway(
    gateway_url: String,
    sign_in_domain: Option<String>,
    webview: tauri::Webview,
    current: tauri::State<'_, Current>,
) -> Result<(), String> {
    if webview.label() != MAIN {
        return Err("not the main window".into());
    }
    let settings = config::validate(&gateway_url, sign_in_domain.as_deref())?;
    let app = webview.app_handle();
    let path = settings_path(app)?;
    config::save(&path, &settings)?;
    log::info!(
        "saved gateway {} to {}",
        config::origin(&settings.gateway_url),
        path.display()
    );
    let url = settings.gateway_url.clone();
    current.apply(app, settings).map_err(|e| e.to_string())?;
    webview.navigate(url).map_err(|e| e.to_string())
}

fn settings_path<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|d| d.join(SETTINGS_FILE))
        .map_err(|e| format!("no app config directory: {e}"))
}

/// Starts the app: the main window (or the setup page when no gateway is configured), which is
/// the whole app on Android. The desktop builds also start the updater and the tray menu.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let args = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    let args = match config::parse_args(args, std::env::var("RD_URL").ok()) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("rd-desktop: {e}");
            std::process::exit(2);
        }
    };

    let builder = tauri::Builder::default()
        .plugin(log_plugin())
        .manage(grab::Grab::default())
        .manage(Current::default())
        .setup(move |app| setup(app, args));

    // The updater plugin is not available on Android (and the app cannot replace itself there).
    #[cfg(desktop)]
    let builder = builder
        // Used from Rust only (update.rs); no page is granted its commands.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_menu_event(|app, event| menus::on_event(app, event.id().as_ref()))
        .invoke_handler(tauri::generate_handler![
            rd_set_grab,
            rd_keyboard_status,
            rd_open_keyboard_settings,
            rd_setup_info,
            rd_save_gateway,
            update::rd_update_state,
            update::rd_update_check,
            update::rd_update_install,
            update::rd_whats_new
        ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        rd_set_grab,
        rd_keyboard_status,
        rd_open_keyboard_settings,
        rd_setup_info,
        rd_save_gateway
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("rd Desktop failed to start");
}

/// Logs go to stdout and to the app log directory (Windows: %LOCALAPPDATA%\dev.thijmen.rd.desktop\logs).
/// `RD_LOG=debug` (or trace/warn/…) changes the level. Keys are never logged.
fn log_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    let level = std::env::var("RD_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    tauri_plugin_log::Builder::new()
        .clear_targets()
        .targets([
            Target::new(TargetKind::Stdout),
            Target::new(TargetKind::LogDir {
                file_name: Some("rd-desktop".into()),
            }),
        ])
        .level(level)
        .level_for("tao", log::LevelFilter::Warn)
        .level_for("wry", log::LevelFilter::Warn)
        .max_file_size(1_000_000)
        .rotation_strategy(RotationStrategy::KeepOne)
        .build()
}

fn setup(app: &mut tauri::App, args: config::Args) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("rd Desktop {}", app.package_info().version);
    let saved = settings_path(app.handle())
        .inspect_err(|e| log::warn!("{e}"))
        .ok()
        .and_then(|p| config::load(&p));
    let settings = match args.url_override {
        Some(url) => {
            log::warn!("gateway overridden for this run: {}", config::origin(&url));
            Some(config::Settings {
                gateway_url: url,
                sign_in_domain: saved.and_then(|s| s.sign_in_domain),
            })
        }
        None => saved,
    };
    let start = match &settings {
        Some(s) if !args.setup => {
            log::info!("loading {}", config::origin(&s.gateway_url));
            WebviewUrl::External(s.gateway_url.clone())
        }
        _ => {
            log::info!("no gateway configured (or --setup): showing the setup page");
            WebviewUrl::App("index.html".into())
        }
    };
    if let Some(s) = settings {
        app.state::<Current>().apply(app, s)?;
    }
    // Before the window: its page loads report to the updater.
    #[cfg(desktop)]
    update::setup(app.handle());

    let nav = app.handle().clone();
    let handle = app.handle().clone();
    #[cfg(all(debug_assertions, desktop))]
    let selftest = selftest::Selftest::from_env(&nav);
    let builder = WebviewWindowBuilder::new(app, MAIN, start)
        .title("rd")
        .inner_size(1280.0, 800.0)
        .min_inner_size(640.0, 400.0)
        .on_navigation(move |url| {
            let settings = nav.state::<Current>().settings();
            if config::allowed_in_window(settings.as_ref(), url) {
                return true;
            }
            open_externally(url);
            false
        })
        .on_page_load(move |_window, payload| {
            // Main frame only. A new page never inherits the grab.
            if payload.event() == PageLoadEvent::Started {
                handle.state::<grab::Grab>().page_changed();
            }
            // The update window waits while a session page is open; the notice after an update.
            #[cfg(desktop)]
            update::main_page(&handle, payload.url(), payload.event());
            #[cfg(all(debug_assertions, desktop))]
            selftest.on_page_load(&_window, &payload);
        });
    // A phone has no window to place, no second window and no file drops.
    #[cfg(desktop)]
    let builder = builder
        .center()
        .on_new_window(|url, _features| {
            // window.open / target=_blank: always the system browser.
            open_externally(&url);
            NewWindowResponse::Deny
        })
        // File transfer (docs/files.md): files dropped on the window reach the page as HTML drag
        // and drop (Tauri's own handler would swallow them), and downloads the page hands over
        // are saved to the Downloads folder (macOS cancels downloads without a handler).
        .disable_drag_drop_handler()
        .on_download(on_download);
    #[cfg(all(debug_assertions, desktop))]
    let builder = selftest::log_titles(builder);
    let window = builder.build()?;

    app.state::<grab::Grab>().attach(&window);
    let handle = app.handle().clone();
    window.on_window_event(move |event| {
        let grab = handle.state::<grab::Grab>();
        match event {
            WindowEvent::Focused(focused) => grab.focus_changed(*focused),
            WindowEvent::CloseRequested { .. } => {
                grab.shutdown();
                // Closing the main window quits the app, also while the update window is open.
                #[cfg(desktop)]
                if let Some(update) = handle.get_webview_window(update::WINDOW) {
                    let _ = update.destroy();
                }
            }
            WindowEvent::Destroyed => grab.shutdown(),
            _ => {}
        }
    });

    #[cfg(desktop)]
    if let Err(e) = menus::setup(app.handle()) {
        log::warn!("no tray menu: {e}");
    }
    #[cfg(windows)]
    if let Err(e) = sysmenu_windows::install(&window) {
        log::warn!("no title-bar menu items: {e}");
    }
    Ok(())
}

/// Saves the downloads of the gateway page (a finished file transfer) to the Downloads folder,
/// under a free name. Paths are not logged; the URL is a `blob:` id.
#[cfg(desktop)]
fn on_download<R: tauri::Runtime>(webview: tauri::Webview<R>, event: DownloadEvent<'_>) -> bool {
    match event {
        DownloadEvent::Requested { url, destination } => {
            let settings = webview.app_handle().state::<Current>().settings();
            if !config::download_allowed(settings.as_ref(), &url) {
                log::warn!("blocked a download from {}", config::origin(&url));
                return false;
            }
            log::info!(
                "saving a download to {}",
                destination
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            true
        }
        DownloadEvent::Finished { success, .. } => {
            if success {
                log::info!("download saved");
            } else {
                log::warn!("download failed");
            }
            true
        }
        _ => true,
    }
}

fn open_externally(url: &Url) {
    if !config::openable_externally(url) {
        log::warn!("blocked navigation to a {} URL", url.scheme());
        return;
    }
    // Android has no `open` crate path yet: an update opens links in Chrome (step 3 of the
    // Android work). Until then the page stays where it is.
    #[cfg(desktop)]
    {
        log::info!("opening {} in the system browser", config::origin(url));
        if let Err(e) = open::that_detached(url.as_str()) {
            log::warn!("cannot open the system browser: {e}");
        }
    }
    #[cfg(mobile)]
    log::warn!(
        "not opening {} in a browser: external links are not wired up on this platform yet",
        config::origin(url)
    );
}

/// Debug builds only: `RD_DESKTOP_SELFTEST=1` makes the loaded gateway page call
/// `rd_set_grab(true)` then `(false)` through `window.__TAURI_INTERNALS__`, and logs what
/// happened (via the document title, which the app does not show).
#[cfg(all(debug_assertions, desktop))]
mod selftest {
    use tauri::webview::{PageLoadEvent, PageLoadPayload};
    use tauri::{AppHandle, Manager as _, Runtime, WebviewWindow, WebviewWindowBuilder};

    // Waits for the session page to set up its bridge, then reports what the page sees and
    // which IPC transport Tauri used (it warns when it falls back to postMessage).
    const SCRIPT: &str = r#"(async () => {
  await new Promise((r) => setTimeout(r, 3000));
  const warnings = [];
  const warn = console.warn;
  console.warn = (...a) => { warnings.push(String(a[0])); warn(...a); };
  const out = { tauri: !!window.__TAURI_INTERNALS__, rdNative: typeof window.rdNative, calls: [] };
  for (const active of [true, false]) {
    try { await window.__TAURI_INTERNALS__.invoke('rd_set_grab', { active }); out.calls.push(active + ': ok'); }
    catch (e) { out.calls.push(active + ': error: ' + e); }
  }
  console.warn = warn;
  out.ipc = warnings.some((w) => w.includes('IPC custom protocol failed')) ? 'postMessage fallback' : 'custom protocol (or fallback already active)';
  out.coi = window.crossOriginIsolated;
  document.title = 'rd-selftest ' + JSON.stringify(out);
})()"#;

    pub struct Selftest(Option<AppHandle>);

    impl Selftest {
        pub fn from_env(app: &AppHandle) -> Self {
            Self(std::env::var_os("RD_DESKTOP_SELFTEST").map(|_| app.clone()))
        }

        pub fn on_page_load<R: Runtime>(
            &self,
            window: &WebviewWindow<R>,
            payload: &PageLoadPayload<'_>,
        ) {
            if let Some(app) = &self.0
                && payload.event() == PageLoadEvent::Finished
                && app
                    .state::<super::Current>()
                    .settings()
                    .is_some_and(|s| payload.url().origin() == s.gateway_url.origin())
            {
                log::info!(
                    "selftest: {} loaded, calling rd_set_grab",
                    payload.url().path()
                );
                let _ = window.eval(SCRIPT);
            }
        }
    }

    pub fn log_titles<'a, R: Runtime, M: tauri::Manager<R>>(
        builder: WebviewWindowBuilder<'a, R, M>,
    ) -> WebviewWindowBuilder<'a, R, M> {
        builder.on_document_title_changed(|_, title| {
            if title.starts_with("rd-selftest ") {
                log::info!("selftest: {title}");
            }
        })
    }
}
