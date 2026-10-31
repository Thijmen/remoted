//! Android-only shell work: the system bars and the screen stay on while a session is on
//! screen, links open in the browser, the APK updates itself from GitHub, and the setup page can
//! be reopened to change the gateway (docs/android.md).
//!
//! The Kotlin half is `gen/android/.../RdAndroidPlugin.kt`; the page half of the update banner
//! is injected into whatever page is loaded (also the gateway's), like `notice.rs` does on the
//! desktop, so no viewer change is needed for it. The manifest, version code and banner text
//! live in `mobile_update.rs`, which is compiled (and tested) on every platform.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager as _, Runtime, Url};

use crate::config;
use crate::mobile_update::{MANIFEST_URL, Manifest, Phase, Release, SCRIPT, banner, release_for};

/// The APK is tens of MB; this only stops a runaway download.
const DOWNLOAD_MAX: u64 = 300 << 20;
const MANIFEST_MAX: u64 = 1 << 20;
/// The setup page, bundled in the app (`bundle.frontendDist`).
const SETUP_URL: &str = "https://tauri.localhost/index.html";

pub fn init() -> TauriPlugin<tauri::Wry> {
    Builder::new("rd-android")
        .setup(|app, api| {
            let handle =
                api.register_android_plugin("dev.thijmen.rd.desktop", "RdAndroidPlugin")?;
            app.manage(Android {
                handle,
                update: Mutex::new(Update::default()),
            });
            // One check per launch; the banner appears on the next page that is not a session.
            let app = app.clone();
            std::thread::Builder::new()
                .name("rd-android-update".into())
                .spawn(move || check_for_update(&app))
                .map_err(|e| {
                    std::io::Error::other(format!("cannot start the update check: {e}"))
                })?;
            Ok(())
        })
        .build()
}

/// The app's view of Android. Managed by the plugin; commands use `State<Android>`.
pub struct Android {
    handle: PluginHandle<tauri::Wry>,
    update: Mutex<Update>,
}

impl Android {
    /// Called from the webview's page-load and navigation callbacks, which run on Android's
    /// main thread; `run_mobile_plugin` blocks until the Kotlin side answers, so it must not
    /// run there (the answer is dispatched on that same thread).
    fn set_chrome(&self, immersive: bool) {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            immersive: bool,
            keep_awake: bool,
        }
        let handle = self.handle.clone();
        tauri::async_runtime::spawn(async move {
            let args = Args {
                immersive,
                // A session should not let the screen sleep either; the page's Wake Lock API
                // covers the browser, but Android may ignore it while the WebView is not visible.
                keep_awake: immersive,
            };
            if let Err(e) = handle
                .run_mobile_plugin_async::<()>("setChrome", args)
                .await
            {
                log::warn!("android: cannot set the window chrome: {e}");
            }
        });
    }

    fn open_url(&self, url: String) {
        #[derive(serde::Serialize)]
        struct Args {
            url: String,
        }
        let handle = self.handle.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = handle
                .run_mobile_plugin_async::<()>("openUrl", Args { url })
                .await
            {
                log::warn!("android: cannot open the browser: {e}");
            }
        });
    }

    /// Runs on the updater's worker thread (never the UI thread): the Kotlin side starts
    /// Android's installer and answers once it has.
    fn install_apk(&self, path: &Path) -> Result<(), String> {
        #[derive(serde::Serialize)]
        struct Args<'a> {
            path: &'a str,
        }
        let path = path.to_string_lossy();
        self.handle
            .run_mobile_plugin::<()>(
                "installApk",
                Args {
                    path: path.as_ref(),
                },
            )
            .map_err(|e| format!("cannot install the update: {e}"))
    }
}

/// What the app knows about a newer APK. `Idle` until the check answers.
#[derive(Default)]
struct Update {
    phase: Phase,
    release: Option<Release>,
    progress: Option<f64>,
    /// The last error, shown in the banner.
    error: Option<String>,
}

/// Called for every main-frame page load: a session page gets the whole screen, and any other
/// page offers a found update.
pub fn page<R: Runtime>(app: &AppHandle<R>, url: &Url, event: PageLoadEvent) {
    let session = app
        .state::<crate::Current>()
        .settings()
        .is_some_and(|s| config::is_session_page(Some(&s), url));
    app.state::<Android>().set_chrome(session);
    if event == PageLoadEvent::Finished && !session {
        push_banner(app);
    }
}

pub fn open_url<R: Runtime>(app: &AppHandle<R>, url: &Url) {
    app.state::<Android>().open_url(url.to_string());
}

/// The setup page, to change the gateway (the viewer's Troubleshoot action).
pub fn open_setup(webview: tauri::Webview) -> Result<(), String> {
    if webview.label() != crate::MAIN {
        return Err("not the main window".into());
    }
    let url = Url::parse(SETUP_URL).map_err(|e| e.to_string())?;
    log::info!("opening the setup page to change the gateway");
    webview.navigate(url).map_err(|e| e.to_string())
}

/// The update banner's buttons: `install` starts the download and the installer, `later` hides
/// the banner until the next launch.
#[tauri::command]
pub async fn rd_android_update(webview: tauri::Webview, action: String) -> Result<(), String> {
    if webview.label() != crate::MAIN {
        return Err("not the main window".into());
    }
    let app = webview.app_handle().clone();
    match action.as_str() {
        "later" => {
            set_phase(&app, Phase::Dismissed, None, None);
            remove_banner(&app);
            Ok(())
        }
        "install" => {
            let release = {
                let android = app.state::<Android>();
                let mut update = android.update.lock().unwrap_or_else(|e| e.into_inner());
                let Some(release) = update.release.clone() else {
                    return Err("there is no update to install".into());
                };
                if update.phase == Phase::Downloading || update.phase == Phase::Installing {
                    return Err("the update is already being installed".into());
                }
                update.phase = Phase::Downloading;
                update.progress = None;
                update.error = None;
                release
            };
            push_banner(&app);
            let task = app.clone();
            tauri::async_runtime::spawn_blocking(move || download_and_install(&task, release))
                .await
                .map_err(|e| e.to_string())?
        }
        other => Err(format!("unknown action {other:?}")),
    }
}

fn check_for_update<R: Runtime>(app: &AppHandle<R>) {
    // Debug builds install from CI on every merge; checking there is noise.
    if cfg!(debug_assertions) {
        log::debug!("android: not checking for updates in a debug build");
        return;
    }
    let current = app.package_info().version.clone();
    let found = match fetch_manifest(&current) {
        Ok(manifest) => {
            let Some(release) = release_for(&manifest, &current) else {
                log::info!(
                    "android: no update (have {current}, offered {})",
                    manifest.version
                );
                return;
            };
            log::info!(
                "android: update {} is available (have {current})",
                release.version
            );
            Some(release)
        }
        Err(e) => {
            log::info!("android: no update: {e}");
            None
        }
    };
    let Some(release) = found else { return };
    {
        let android = app.state::<Android>();
        let mut update = android.update.lock().unwrap_or_else(|e| e.into_inner());
        update.release = Some(release);
        update.phase = Phase::Available;
    }
    push_banner(app);
}

fn fetch_manifest(current: &semver::Version) -> anyhow::Result<Manifest> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(format!("rd-desktop/{current} (android)"))
        .https_only(true)
        .max_redirects(10)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .get(MANIFEST_URL)
        .config()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .call()?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MANIFEST_MAX)
        .read_to_vec()?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn download_and_install<R: Runtime>(app: &AppHandle<R>, release: Release) -> Result<(), String> {
    match download(app, &release) {
        Ok(path) => {
            set_phase(app, Phase::Installing, Some(1.0), None);
            push_banner(app);
            log::info!("android: opening the installer for {}", release.version);
            let result = app.state::<Android>().install_apk(&path);
            if let Err(e) = &result {
                log::warn!("android: {e}");
                set_phase(app, Phase::Failed, None, Some(e.clone()));
                push_banner(app);
            }
            result
        }
        Err(e) => {
            log::warn!("android: the update download failed: {e}");
            set_phase(app, Phase::Failed, None, Some(e.clone()));
            push_banner(app);
            Err(e)
        }
    }
}

fn download<R: Runtime>(app: &AppHandle<R>, release: &Release) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no cache directory: {e}"))?
        .join("updates");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let partial = dir.join(format!("rd-{}.apk.part", release.version));
    let apk = dir.join(format!("rd-{}.apk", release.version));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(format!(
            "rd-desktop/{} (android)",
            env!("CARGO_PKG_VERSION")
        ))
        .https_only(true)
        .max_redirects(10)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .get(&release.url)
        .config()
        .timeout_global(Some(Duration::from_secs(30 * 60)))
        .build()
        .call()
        .map_err(|e| format!("cannot download the update: {e}"))?;
    let total = response.body().content_length();
    if total.is_some_and(|n| n > DOWNLOAD_MAX) {
        return Err("the update is larger than expected".into());
    }
    let mut reader = response.body_mut().as_reader();
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(&partial).map_err(|e| format!("cannot write the update: {e}"))?,
    );
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    let mut pushed = Instant::now();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("cannot read the update: {e}"))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > DOWNLOAD_MAX {
            let _ = std::fs::remove_file(&partial);
            return Err("the update is larger than expected".into());
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("cannot write the update: {e}"))?;
        if pushed.elapsed() >= Duration::from_millis(200) {
            pushed = Instant::now();
            let progress = total.filter(|t| *t > 0).map(|t| written as f64 / t as f64);
            set_phase(app, Phase::Downloading, progress, None);
            push_banner(app);
        }
    }
    out.flush()
        .map_err(|e| format!("cannot write the update: {e}"))?;
    drop(out);
    let digest = hex::encode(hasher.finalize());
    if !digest.eq_ignore_ascii_case(&release.sha256) {
        let _ = std::fs::remove_file(&partial);
        return Err("the downloaded update is not the one that was announced".into());
    }
    std::fs::rename(&partial, &apk).map_err(|e| format!("cannot keep the update: {e}"))?;
    Ok(apk)
}

fn set_phase<R: Runtime>(
    app: &AppHandle<R>,
    phase: Phase,
    progress: Option<f64>,
    error: Option<String>,
) {
    let android = app.state::<Android>();
    let mut update = android.update.lock().unwrap_or_else(|e| e.into_inner());
    update.phase = phase;
    update.progress = progress;
    update.error = error;
}

fn push_banner<R: Runtime>(app: &AppHandle<R>) {
    let current = app.package_info().version.clone();
    let view = {
        let android = app.state::<Android>();
        let update = android.update.lock().unwrap_or_else(|e| e.into_inner());
        banner(
            update.phase,
            update.release.as_ref(),
            &current,
            update.progress,
            update.error.as_deref(),
        )
    };
    if !view.show {
        if matches!(app.state::<Android>().phase(), Phase::Dismissed) {
            remove_banner(app);
        }
        return;
    }
    let Ok(json) = serde_json::to_string(&view) else {
        return;
    };
    let js = SCRIPT.replace("__RD_STATE__", &json);
    // A page that is still loading, or a session page whose banner we do not want, may fail:
    // it is cosmetic.
    if let Some(window) = app.get_webview_window(crate::MAIN) {
        let _ = window.eval(js);
    }
}

impl Android {
    fn phase(&self) -> Phase {
        self.update.lock().unwrap_or_else(|e| e.into_inner()).phase
    }
}

fn remove_banner<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(crate::MAIN) {
        let _ = window
            .eval("(()=>{const e=document.getElementById('rd-android-update');if(e)e.remove()})()");
    }
}
