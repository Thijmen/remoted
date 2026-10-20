//! Updates (docs/updates.md, "rd Desktop").
//!
//! 10 s after launch and then every 6 hours, unless automatic checks are off, the app reads the
//! update manifest. A newer version is downloaded in the background and its signature checked
//! (the updater plugin refuses files not signed by the key in `tauri.conf.json`). Only then does
//! the update window (`dist/update.html`) open, and never over a session page (`/m/…`): it waits
//! until that page is left, or for the next check. Closing the window means "later". Its one
//! button installs and restarts; copies that cannot replace themselves (`installable.rs`) get a
//! button that opens the release page instead.
//!
//! The window is a bundled page with its own capability (`capabilities/update.json`): it reads
//! the state with `rd_update_state` and gets every change pushed as `window.rdUpdate(view)`.
//! Links in it open in the system browser.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use semver::Version;
use serde::Serialize;
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::installable::{self, Blocker};
use crate::{config, notes, notice};

/// Label of the update window (capabilities/update.json).
pub const WINDOW: &str = "update";

const FIRST_CHECK: Duration = Duration::from_secs(10);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// After a failed automatic check (offline at launch, a laptop waking up).
const RETRY: Duration = Duration::from_secs(30 * 60);
/// How often the timer looks at the wall clock. Sleep timers stop while the computer sleeps;
/// the clock does not.
const TICK: Duration = Duration::from_secs(60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// A whole download (the macOS archive is about 15 MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long the "Updated to X.Y.Z" notice stays.
const NOTICE: Duration = Duration::from_secs(20);
/// Release pages when the manifest has none (the tag scheme of `desktop.yml`).
const RELEASE_PAGE: &str = "https://github.com/Thijmen/remoted/releases/tag/desktop-v";

/// What the update window shows besides the update itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// The update: found, downloading, ready, or the result of a manual check.
    Update,
    /// After an update: the notes of the versions it brought ("What's new").
    Notes,
    /// Windows' title-bar menu "About rd Desktop": the version and the update state.
    About,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Self::Update => "rd Desktop update",
            Self::Notes => "What's new in rd Desktop",
            Self::About => "About rd Desktop",
        }
    }
}

/// Who started a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// The timer: quiet unless an update is ready.
    Auto,
    /// "Check for Updates…" (menus, the window's button): results and errors show in the window.
    Manual,
    /// The notes window needs the manifest: errors show in the window, an update waits for the
    /// next automatic check to be offered.
    Notes,
}

enum Phase {
    Idle,
    Checking,
    Downloading {
        version: Version,
        got: u64,
        total: Option<u64>,
    },
    /// Downloaded and verified.
    Ready {
        update: Box<Update>,
        bytes: Arc<Vec<u8>>,
    },
    /// Newer version, but this copy cannot install it.
    Manual {
        version: Version,
    },
    UpToDate,
    Failed(String),
    Installing {
        version: Version,
    },
}

impl Phase {
    /// The newer version this phase is about.
    fn version(&self) -> Option<Version> {
        match self {
            Self::Downloading { version, .. }
            | Self::Manual { version }
            | Self::Installing { version } => Some(version.clone()),
            Self::Ready { update, .. } => Version::parse(&update.version).ok(),
            _ => None,
        }
    }
}

struct Inner {
    phase: Phase,
    /// The `releases` of the last manifest read.
    releases: Vec<notes::Release>,
    mode: Mode,
    /// "Check for updates automatically" (settings.json).
    auto: bool,
    /// A check or download is running.
    busy: bool,
    /// The main window shows a session page.
    in_session: bool,
    /// An update is ready, but a session page was open: open the window once it is left.
    deferred: bool,
    /// When the timer checks next (`None`: right away, the first check after launch).
    next_check: Option<SystemTime>,
    /// The notice "Updated to X.Y.Z" is visible until then (set when first shown).
    notice_until: Option<Instant>,
    /// Numbers the views pushed to the window, so a late answer cannot replace a newer one.
    seq: u64,
    /// The tray item's text, to change it only when needed.
    menu_text: String,
}

/// Managed state: everything about updates.
pub struct Updates {
    inner: Mutex<Inner>,
    current: Version,
    /// `RD_UPDATE_URL`: replaces the endpoints of `tauri.conf.json`.
    endpoint: Option<Url>,
    /// `RD_UPDATE_CHECK=0`: no automatic checks, whatever the settings say.
    forced_off: bool,
    /// Debug builds check automatically only with `RD_UPDATE_URL` (a development build is
    /// always older than the latest release).
    dev_quiet: bool,
    blocker: Option<Blocker>,
    settings_path: Option<PathBuf>,
    /// This launch is the first one after an update from this version.
    updated_from: Option<Version>,
}

/// What the window renders (`dist/update.js`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    seq: u64,
    mode: Mode,
    /// The installed version.
    current: String,
    /// idle, checking, downloading, ready, manual, upToDate, failed, installing.
    status: &'static str,
    /// The newer version (update, about), or the one updated to (notes).
    version: Option<String>,
    /// Notes mode: the version updated from.
    previous: Option<String>,
    /// While downloading, if the size is known: 0..=1.
    progress: Option<f64>,
    error: Option<String>,
    /// Why this copy cannot update itself.
    blocker: Option<String>,
    notes: Vec<notes::Shown>,
    release_url: String,
}

impl Updates {
    /// Reads the settings and the environment, and records this launch.
    pub fn new(app: &AppHandle) -> Self {
        let current = app.package_info().version.clone();
        let settings_path = crate::settings_path(app)
            .inspect_err(|e| log::warn!("{e}"))
            .ok();
        let auto = settings_path
            .as_deref()
            .is_none_or(config::load_update_check);
        let endpoint = std::env::var("RD_UPDATE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .and_then(|raw| {
                update_url(&raw)
                    .inspect(|u| log::warn!("update manifest overridden: {u}"))
                    .inspect_err(|e| log::warn!("ignoring RD_UPDATE_URL: {e}"))
                    .ok()
            });
        let forced_off = checks_forced_off(std::env::var("RD_UPDATE_CHECK").ok().as_deref());
        let dev_quiet = cfg!(debug_assertions) && endpoint.is_none();
        let blocker = installable::blocker();
        let updated_from = app
            .path()
            .app_local_data_dir()
            .inspect_err(|e| log::warn!("no app data directory: {e}"))
            .ok()
            .and_then(|dir| notice::record_launch(&dir.join(notice::STATE_FILE), &current));

        if forced_off {
            log::info!("automatic update checks are off (RD_UPDATE_CHECK)");
        } else if dev_quiet {
            log::info!("automatic update checks are off (debug build without RD_UPDATE_URL)");
        } else if !auto {
            log::info!("automatic update checks are off (settings)");
        }
        if let Some(b) = &blocker {
            log::info!("this copy cannot update itself ({b:?}): updates are offered as downloads");
        }
        if let Some(from) = &updated_from {
            log::info!("updated from {from} to {current}");
        }
        Self {
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                releases: Vec::new(),
                mode: Mode::Update,
                auto,
                busy: false,
                in_session: false,
                deferred: false,
                next_check: None,
                notice_until: None,
                seq: 0,
                menu_text: String::new(),
            }),
            current,
            endpoint,
            forced_off,
            dev_quiet,
            blocker,
            settings_path,
            updated_from,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// For the tray menu: (checked, enabled) of "Check for updates automatically". Forced off
    /// by `RD_UPDATE_CHECK`, it shows unchecked and cannot be changed.
    pub fn auto_item(&self) -> (bool, bool) {
        if self.forced_off {
            (false, false)
        } else {
            (self.lock().auto, true)
        }
    }

    pub fn version(&self) -> &Version {
        &self.current
    }

    fn auto_enabled(&self, inner: &Inner) -> bool {
        !self.forced_off && !self.dev_quiet && inner.auto
    }

    fn view(&self, inner: &mut Inner) -> View {
        inner.seq += 1;
        let (status, error, progress) = match &inner.phase {
            Phase::Idle => ("idle", None, None),
            Phase::Checking => ("checking", None, None),
            Phase::Downloading { got, total, .. } => (
                "downloading",
                None,
                total
                    .filter(|t| *t > 0)
                    .map(|t| (*got as f64 / t as f64).min(1.0)),
            ),
            Phase::Ready { .. } => ("ready", None, None),
            Phase::Manual { .. } => ("manual", None, None),
            Phase::UpToDate => ("upToDate", None, None),
            Phase::Failed(e) => ("failed", Some(e.clone()), None),
            Phase::Installing { .. } => ("installing", None, None),
        };
        let found = inner.phase.version();
        let (version, previous, notes) = match inner.mode {
            Mode::Notes => {
                // The versions this launch brought; without an update, this version's own notes.
                let after = self.updated_from.clone().unwrap_or_else(|| {
                    inner
                        .releases
                        .iter()
                        .map(|r| r.version.clone())
                        .find(|v| *v < self.current)
                        .unwrap_or_else(|| Version::new(0, 0, 0))
                });
                let notes = notes::shown(&inner.releases, &after, Some(&self.current));
                (
                    Some(self.current.to_string()),
                    self.updated_from.as_ref().map(Version::to_string),
                    notes,
                )
            }
            Mode::Update | Mode::About => {
                let notes = found
                    .as_ref()
                    .map(|v| notes::shown(&inner.releases, &self.current, Some(v)))
                    .unwrap_or_default();
                (found.as_ref().map(Version::to_string), None, notes)
            }
        };
        let page = match inner.mode {
            Mode::Notes => self.current.clone(),
            Mode::Update | Mode::About => found.unwrap_or_else(|| self.current.clone()),
        };
        let release_url = release_url(&inner.releases, &page);
        View {
            seq: inner.seq,
            mode: inner.mode,
            current: self.current.to_string(),
            status,
            version,
            previous,
            progress,
            error,
            blocker: matches!(inner.phase, Phase::Manual { .. })
                .then(|| self.blocker.as_ref().map(Blocker::describe))
                .flatten(),
            notes,
            release_url,
        }
    }

    /// The state changed: update the window (if open) and the menus.
    fn changed(&self, app: &AppHandle) {
        self.push(app);
        let text = {
            let mut inner = self.lock();
            let text = match &inner.phase {
                Phase::Ready { update, .. } => format!("Restart to Update to {}…", update.version),
                Phase::Manual { version } => format!("Download rd Desktop {version}…"),
                _ => "Check for Updates…".to_string(),
            };
            if inner.menu_text == text {
                return;
            }
            inner.menu_text = text.clone();
            text
        };
        crate::menus::set_update_text(app, &text);
    }

    fn push(&self, app: &AppHandle) {
        let Some(window) = app.get_webview_window(WINDOW) else {
            return;
        };
        let view = self.view(&mut self.lock());
        match serde_json::to_string(&view) {
            Ok(json) => {
                if let Err(e) = window.eval(format!("window.rdUpdate&&window.rdUpdate({json})")) {
                    log::debug!("update window: {e}");
                }
            }
            Err(e) => log::warn!("update window: {e}"),
        }
    }
}

/// The GitHub release page of `version`: from the manifest, else by the tag scheme.
fn release_url(releases: &[notes::Release], version: &Version) -> String {
    releases
        .iter()
        .find(|r| r.version == *version)
        .and_then(|r| r.url.clone())
        .unwrap_or_else(|| format!("{RELEASE_PAGE}{version}"))
}

/// Registers the state and starts the timer.
pub fn setup(app: &AppHandle) {
    app.manage(Updates::new(app));
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("rd-update-timer".into())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK);
            loop {
                let updates = app.state::<Updates>();
                let due = {
                    let inner = updates.lock();
                    updates.auto_enabled(&inner)
                        && !inner.busy
                        && is_due(inner.next_check, SystemTime::now())
                };
                if due {
                    check(&app, Trigger::Auto);
                }
                std::thread::sleep(TICK);
            }
        });
    if let Err(e) = spawned {
        log::warn!("no automatic update checks: {e}");
    }
}

/// Is the next automatic check due? `None`: not checked yet. A clock set back by more than an
/// interval also makes it due, so a wrong clock cannot stop the checks.
fn is_due(next: Option<SystemTime>, now: SystemTime) -> bool {
    next.is_none_or(|t| {
        now >= t
            || t.duration_since(now)
                .is_ok_and(|ahead| ahead > INTERVAL + TICK)
    })
}

/// "Check for Updates…" / "Restart to Update to X.Y.Z…": opens the window, and checks unless an
/// update is already waiting there.
pub fn check_now(app: &AppHandle) {
    open_window(app, Mode::Update, true);
    let ready = matches!(
        app.state::<Updates>().lock().phase,
        Phase::Ready { .. } | Phase::Manual { .. } | Phase::Installing { .. }
    );
    if !ready {
        check(app, Trigger::Manual);
    }
}

/// Windows' "About rd Desktop" (macOS has the standard About panel).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn about(app: &AppHandle) {
    open_window(app, Mode::About, true);
}

/// The tray's "Check for updates automatically".
pub fn set_auto(app: &AppHandle, on: bool) {
    let updates = app.state::<Updates>();
    updates.lock().auto = on;
    log::info!(
        "automatic update checks {} (tray menu)",
        if on { "on" } else { "off" }
    );
    if let Some(path) = &updates.settings_path
        && let Err(e) = config::save_update_check(path, on)
    {
        log::warn!("{e}");
    }
}

fn check(app: &AppHandle, trigger: Trigger) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move { run_check(&app, trigger).await });
}

async fn run_check(app: &AppHandle, trigger: Trigger) {
    let updates = app.state::<Updates>();
    {
        let mut inner = updates.lock();
        if inner.busy {
            // The running check updates the window when it is done.
            return;
        }
        inner.busy = true;
        // A ready update stays offered while the manifest is read again.
        if !matches!(inner.phase, Phase::Ready { .. }) {
            inner.phase = Phase::Checking;
        }
    }
    updates.changed(app);
    let kind = match trigger {
        Trigger::Auto => "automatic",
        Trigger::Manual => "manual",
        Trigger::Notes => "release notes",
    };

    let found = fetch(app, &updates).await;
    let now = SystemTime::now();
    let update = match found {
        Err(e) => {
            log::warn!("update check ({kind}) failed: {e}");
            let mut inner = updates.lock();
            if trigger == Trigger::Auto {
                inner.next_check = Some(now + RETRY);
                if !matches!(inner.phase, Phase::Ready { .. }) {
                    inner.phase = Phase::Idle;
                }
            } else {
                inner.phase = Phase::Failed(format!("Cannot check for updates: {e}"));
            }
            inner.busy = false;
            drop(inner);
            updates.changed(app);
            return;
        }
        Ok((releases, update)) => {
            let mut inner = updates.lock();
            inner.releases = releases;
            inner.next_check = Some(now + INTERVAL);
            match update {
                Some(update) => update,
                None => {
                    log::info!("update check ({kind}): {} is up to date", updates.current);
                    inner.phase = Phase::UpToDate;
                    inner.busy = false;
                    drop(inner);
                    updates.changed(app);
                    return;
                }
            }
        }
    };

    let Ok(version) = Version::parse(&update.version) else {
        // The plugin parsed it already; cannot happen.
        updates.lock().busy = false;
        return;
    };
    log::info!(
        "update check ({kind}): rd Desktop {version} is available (installed: {})",
        updates.current
    );
    let have = updates.lock().phase.version();
    if updates.blocker.is_some() {
        updates.lock().phase = Phase::Manual { version };
    } else if have.as_ref() == Some(&version) && matches!(updates.lock().phase, Phase::Ready { .. })
    {
        // Downloaded before; the window was closed ("later").
    } else {
        match download(app, &updates, update, version.clone()).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("downloading rd Desktop {version} failed: {e}");
                let mut inner = updates.lock();
                inner.phase = if trigger == Trigger::Auto {
                    inner.next_check = Some(SystemTime::now() + RETRY);
                    Phase::Idle
                } else {
                    Phase::Failed(format!("Cannot download rd Desktop {version}: {e}"))
                };
                inner.busy = false;
                drop(inner);
                updates.changed(app);
                return;
            }
        }
    }
    updates.lock().busy = false;
    updates.changed(app);
    // A manual check shows its result in the window it opened.
    if trigger != Trigger::Manual {
        offer(app);
    }
}

/// Reads the manifest. The comparator accepts every version, so the manifest (and its release
/// notes) also comes back when there is no update; whether it is one is decided here.
async fn fetch(
    app: &AppHandle,
    updates: &Updates,
) -> Result<(Vec<notes::Release>, Option<Update>), String> {
    let mut builder = app
        .updater_builder()
        .version_comparator(|_, _| true)
        .timeout(CHECK_TIMEOUT);
    if let Some(url) = &updates.endpoint {
        builder = builder.endpoints(vec![url.clone()]).map_err(|e| {
            if url.scheme() == "http" {
                format!("{e} (plain http:// update URLs only work in debug builds)")
            } else {
                e.to_string()
            }
        })?;
    }
    let updater = builder.build().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        // 204 No Content.
        return Ok((Vec::new(), None));
    };
    let releases = notes::releases(&update.raw_json);
    let newer = Version::parse(&update.version).is_ok_and(|v| v > updates.current);
    Ok((releases, newer.then_some(update)))
}

async fn download(
    app: &AppHandle,
    updates: &Updates,
    mut update: Update,
    version: Version,
) -> Result<(), String> {
    updates.lock().phase = Phase::Downloading {
        version: version.clone(),
        got: 0,
        total: None,
    };
    updates.changed(app);
    log::info!(
        "downloading rd Desktop {version} from {}",
        config::origin(&update.download_url)
    );
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    let mut got = 0u64;
    let mut pushed = Instant::now();
    let bytes = update
        .download(
            |chunk, total| {
                got += chunk as u64;
                if let Phase::Downloading {
                    got: g, total: t, ..
                } = &mut updates.lock().phase
                {
                    *g = got;
                    *t = total;
                }
                if pushed.elapsed() >= Duration::from_millis(250) {
                    pushed = Instant::now();
                    updates.push(app);
                }
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    log::info!(
        "downloaded rd Desktop {version} ({} bytes), signature verified",
        bytes.len()
    );
    updates.lock().phase = Phase::Ready {
        update: Box::new(update),
        bytes: Arc::new(bytes),
    };
    Ok(())
}

/// An update is ready (or can be downloaded by hand): open the window, unless a session is open
/// in the main window. The window is not focused, so typing elsewhere goes on.
fn offer(app: &AppHandle) {
    let updates = app.state::<Updates>();
    let open = {
        let mut inner = updates.lock();
        let what = match &inner.phase {
            Phase::Ready { .. } => "update ready",
            Phase::Manual { .. } => "update available (to download by hand)",
            _ => return,
        };
        if app.get_webview_window(WINDOW).is_some() {
            false
        } else if inner.in_session {
            inner.deferred = true;
            log::info!("{what}: waiting until the session page is left");
            false
        } else {
            log::info!("{what}: opening the update window");
            true
        }
    };
    if open {
        open_window(app, Mode::Update, false);
    }
}

/// The main window started or finished loading a page.
pub fn main_page(app: &AppHandle, url: &Url, event: PageLoadEvent) {
    let Some(updates) = app.try_state::<Updates>() else {
        return;
    };
    match event {
        PageLoadEvent::Started => {
            let settings = app.state::<crate::Current>().settings();
            let in_session = config::is_session_page(settings.as_ref(), url);
            let offer_now = {
                let mut inner = updates.lock();
                inner.in_session = in_session;
                !in_session && std::mem::take(&mut inner.deferred)
            };
            if offer_now {
                log::info!("session page left: offering the update");
                offer(app);
            }
        }
        PageLoadEvent::Finished => {
            let Some(script) = notice_script(&updates) else {
                return;
            };
            if let Some(window) = app.get_webview_window(crate::MAIN)
                && let Err(e) = window.eval(script)
            {
                log::warn!("cannot show the update notice: {e}");
            }
        }
    }
}

/// The notice after an update, for the time it has left (it starts with the first page shown
/// and comes back on the pages loaded within those 20 s).
fn notice_script(updates: &Updates) -> Option<String> {
    updates.updated_from.as_ref()?;
    let now = Instant::now();
    let mut inner = updates.lock();
    let until = *inner.notice_until.get_or_insert(now + NOTICE);
    let left = until.checked_duration_since(now)?;
    if left < Duration::from_secs(2) {
        return None;
    }
    let url = release_url(&inner.releases, &updates.current);
    Some(notice::script(&updates.current.to_string(), &url, left))
}

/// Opens (or switches and shows) the update window. Built from a task: creating a window in a
/// menu or window event handler would deadlock on Windows.
fn open_window(app: &AppHandle, mode: Mode, focus: bool) {
    let updates = app.state::<Updates>();
    updates.lock().mode = mode;
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.set_title(mode.title());
        updates.push(app);
        if focus {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let built = WebviewWindowBuilder::new(&app, WINDOW, WebviewUrl::App("update.html".into()))
            .title(mode.title())
            .inner_size(480.0, 560.0)
            .min_inner_size(380.0, 320.0)
            .maximizable(false)
            .center()
            .focused(focus)
            .on_navigation(|url| {
                if config::is_app_page(url) {
                    return true;
                }
                crate::open_externally(url);
                false
            })
            .on_new_window(|url, _features| {
                crate::open_externally(&url);
                NewWindowResponse::Deny
            })
            .build();
        if let Err(e) = built {
            log::warn!("cannot open the update window: {e}");
        }
    });
}

/// Update window -> app: what to show.
#[tauri::command]
pub fn rd_update_state(
    webview: tauri::Webview,
    updates: tauri::State<'_, Updates>,
) -> Result<View, String> {
    if webview.label() != WINDOW {
        return Err("not the update window".into());
    }
    Ok(updates.view(&mut updates.lock()))
}

/// Update window -> app: "Check for Updates", "Check again", "Try again".
#[tauri::command]
pub fn rd_update_check(webview: tauri::Webview) -> Result<(), String> {
    if webview.label() != WINDOW {
        return Err("not the update window".into());
    }
    check(webview.app_handle(), Trigger::Manual);
    Ok(())
}

/// Update window -> app: "Restart & update" installs the downloaded update and restarts;
/// "Download" (this copy cannot update itself) opens the release page.
#[tauri::command]
pub async fn rd_update_install(webview: tauri::Webview) -> Result<(), String> {
    if webview.label() != WINDOW {
        return Err("not the update window".into());
    }
    let app = webview.app_handle().clone();
    let updates = app.state::<Updates>();
    let ready = {
        let mut inner = updates.lock();
        match &inner.phase {
            Phase::Ready { update, bytes } => {
                let ready = (update.clone(), bytes.clone());
                if let Some(version) = inner.phase.version() {
                    inner.phase = Phase::Installing { version };
                }
                ready
            }
            Phase::Manual { version } => {
                let page = release_url(&inner.releases, version);
                drop(inner);
                if let Ok(url) = Url::parse(&page) {
                    crate::open_externally(&url);
                }
                return Ok(());
            }
            _ => return Err("no update is ready".into()),
        }
    };
    updates.changed(&app);
    let (update, bytes) = ready;
    log::info!("installing rd Desktop {}", update.version);
    // Windows: starts the installer (passive: a progress bar only) and exits; the installer
    // starts the new version with the same arguments. macOS: swaps the app bundle.
    let installed = tauri::async_runtime::spawn_blocking(move || update.install(bytes.as_slice()))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    match installed {
        Ok(()) => {
            log::info!("update installed: restarting");
            app.restart();
        }
        Err(e) => {
            log::warn!("installing the update failed: {e}");
            updates.lock().phase = Phase::Failed(format!("The update could not be installed: {e}"));
            updates.changed(&app);
            Err(e)
        }
    }
}

/// Main window (gateway page, setup page) -> app: the notice's "What's new". Opens the update
/// window with the notes of the versions this launch brought. Harmless: it shows a local
/// window and reads the public manifest.
#[tauri::command]
pub async fn rd_whats_new(webview: tauri::Webview) -> Result<(), String> {
    if webview.label() != crate::MAIN {
        return Err("not the main window".into());
    }
    let app = webview.app_handle();
    open_window(app, Mode::Notes, true);
    let updates = app.state::<Updates>();
    let need = {
        let inner = updates.lock();
        inner.releases.is_empty() && !inner.busy
    };
    if need {
        check(app, Trigger::Notes);
    }
    Ok(())
}

/// `RD_UPDATE_CHECK=0` (or false/off/no) turns automatic checks off, whatever the settings say.
fn checks_forced_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// An `RD_UPDATE_URL` value: https, or plain http to this machine (which the updater accepts in
/// debug builds only).
fn update_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("invalid URL {raw:?}: {e}"))?;
    let host = url.host_str().unwrap_or_default();
    match url.scheme() {
        "https" if !host.is_empty() => {}
        "http" if config::is_loopback(host) => {}
        _ => {
            return Err(format!(
                "{raw:?}: use https:// (plain http only for localhost)"
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{raw:?}: no user name or password in the URL"));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_off() {
        for v in ["0", "false", "OFF", " no "] {
            assert!(checks_forced_off(Some(v)), "{v}");
        }
        for v in ["1", "", "yes", "on"] {
            assert!(!checks_forced_off(Some(v)), "{v}");
        }
        assert!(!checks_forced_off(None));
    }

    #[test]
    fn override_urls() {
        assert!(update_url("https://example.com/desktop.json").is_ok());
        assert!(update_url(" http://127.0.0.1:8123/desktop.json ").is_ok());
        assert!(update_url("http://localhost:8123/desktop.json").is_ok());
        assert!(update_url("http://example.com/desktop.json").is_err());
        assert!(update_url("file:///tmp/desktop.json").is_err());
        assert!(update_url("https://u:p@example.com/desktop.json").is_err());
        assert!(update_url("desktop.json").is_err());
    }

    #[test]
    fn checks_are_due() {
        let now = SystemTime::now();
        assert!(is_due(None, now), "first check after launch");
        assert!(is_due(Some(now), now));
        assert!(is_due(Some(now - Duration::from_secs(1)), now));
        assert!(!is_due(Some(now + Duration::from_secs(60)), now));
        assert!(!is_due(Some(now + INTERVAL), now));
        // The clock went back a day: check now rather than in a day and 6 hours.
        assert!(is_due(
            Some(now + INTERVAL + Duration::from_secs(86_400)),
            now
        ));
    }
}
