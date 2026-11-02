//! macOS: what happens when the web process of a page ends.
//!
//! WKWebView runs each page in a separate web process. When that process crashes, or macOS ends
//! it, the window goes blank. Tauri's default then reloads the page at once, every time: a page
//! whose process ends on every load reloads in a tight loop, which hangs the app (spinning cursor)
//! and fetches the page and its API calls again on every round, enough to swamp the gateway and
//! the sign-in proxy in front of it until the app quits.
//!
//! Instead, per window and within [`WINDOW`]: the first end reloads at once, the second after
//! [`SECOND_RELOAD_DELAY`], and from the third on the main window shows `stopped.html` (bundled),
//! which says what happened and offers to try again. Every end is logged.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Ends this close together count together.
pub const WINDOW: Duration = Duration::from_secs(60);
/// The wait before the second reload within [`WINDOW`].
pub const SECOND_RELOAD_DELAY: Duration = Duration::from_secs(2);
/// From this many ends within [`WINDOW`] on, the page is not reloaded any more.
pub const GIVE_UP_AT: usize = 3;

/// What to do after a page's web process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Reload the page after this long.
    Reload(Duration),
    /// Stop reloading (the main window shows the stopped page).
    Stop,
}

/// Recent ends of one window's web process.
#[derive(Debug, Default)]
pub struct Ends {
    recent: VecDeque<Instant>,
}

impl Ends {
    /// The web process ended at `now`. Returns how many times it ended within [`WINDOW`] (this
    /// time included, at most [`GIVE_UP_AT`]) and what to do.
    pub fn ended(&mut self, now: Instant) -> (usize, Recovery) {
        while self
            .recent
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= WINDOW)
        {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        // Older ends no longer change the answer.
        while self.recent.len() > GIVE_UP_AT {
            self.recent.pop_front();
        }
        let n = self.recent.len();
        let recovery = match n {
            1 => Recovery::Reload(Duration::ZERO),
            n if n < GIVE_UP_AT => Recovery::Reload(SECOND_RELOAD_DELAY),
            _ => Recovery::Stop,
        };
        (n, recovery)
    }
}

/// The ends per window (webview label).
#[derive(Default)]
pub struct WebProcesses(Mutex<HashMap<String, Ends>>);

impl WebProcesses {
    fn ended(&self, label: &str, now: Instant) -> (usize, Recovery) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(label.to_string())
            .or_default()
            .ended(now)
    }
}

/// The bundled page the main window shows instead of reloading again: `?url=` is the page whose
/// process kept ending (its "Try again").
fn stopped_page(page: &tauri::Url) -> Option<tauri::Url> {
    // App pages are tauri://localhost/… on macOS (`config::is_app_page`).
    let mut url = tauri::Url::parse("tauri://localhost/stopped.html").ok()?;
    url.query_pairs_mut().append_pair("url", page.as_str());
    Some(url)
}

/// `scheme://host[:port]/path` of a page, for the log (no query or fragment).
fn describe(url: &tauri::Url) -> String {
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{}{port}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.path()
    )
}

/// The hook for `tauri::Builder::on_web_content_process_terminate` (replaces Tauri's reload).
#[cfg(target_os = "macos")]
pub fn on_terminate<R: tauri::Runtime>(webview: &tauri::Webview<R>) {
    use tauri::Manager as _;

    let label = webview.label().to_string();
    let url = webview.url().ok();
    let (n, recovery) = webview
        .app_handle()
        .state::<WebProcesses>()
        .ended(&label, Instant::now());
    let page = url.as_ref().map_or_else(|| "a page".to_string(), describe);
    let times = if n == 1 {
        "once".to_string()
    } else {
        format!("{n} times within a minute")
    };
    match recovery {
        Recovery::Reload(after) if after.is_zero() => {
            log::warn!("the web process of {page} ended ({times}); reloading it");
            reload(webview);
        }
        Recovery::Reload(after) => {
            log::warn!(
                "the web process of {page} ended ({times}); reloading it in {} s",
                after.as_secs()
            );
            let webview = webview.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                reload(&webview);
            });
        }
        Recovery::Stop => {
            // Bundled pages (setup, update, the stopped page itself) have nothing better to show.
            let target = url
                .as_ref()
                .filter(|u| label == crate::MAIN && !crate::config::is_app_page(u))
                .and_then(stopped_page);
            log::warn!(
                "the web process of {page} ended ({times}); not reloading it again{}",
                if target.is_some() {
                    " (showing the stopped page)"
                } else {
                    ""
                }
            );
            if let Some(target) = target
                && let Err(e) = webview.navigate(target)
            {
                log::error!("cannot show the stopped page: {e}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn reload<R: tauri::Runtime>(webview: &tauri::Webview<R>) {
    if let Err(e) = webview.reload() {
        log::error!("cannot reload the page: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloads_twice_then_stops() {
        let mut ends = Ends::default();
        let t0 = Instant::now();
        assert_eq!(ends.ended(t0), (1, Recovery::Reload(Duration::ZERO)));
        assert_eq!(
            ends.ended(t0 + Duration::from_millis(500)),
            (2, Recovery::Reload(SECOND_RELOAD_DELAY))
        );
        assert_eq!(ends.ended(t0 + Duration::from_secs(3)), (3, Recovery::Stop));
        // Trying again from the stopped page within the minute stops at once.
        assert_eq!(
            ends.ended(t0 + Duration::from_secs(20)),
            (GIVE_UP_AT, Recovery::Stop)
        );
    }

    #[test]
    fn a_quiet_minute_starts_over() {
        let mut ends = Ends::default();
        let t0 = Instant::now();
        for i in 0..5 {
            ends.ended(t0 + Duration::from_secs(i));
        }
        assert_eq!(
            ends.ended(t0 + Duration::from_secs(4) + WINDOW),
            (1, Recovery::Reload(Duration::ZERO))
        );
    }

    #[test]
    fn ends_further_apart_than_the_window_always_reload_at_once() {
        let mut ends = Ends::default();
        let t0 = Instant::now();
        for i in 0..10 {
            assert_eq!(
                ends.ended(t0 + WINDOW * i),
                (1, Recovery::Reload(Duration::ZERO))
            );
        }
    }

    #[test]
    fn windows_are_counted_apart() {
        let p = WebProcesses::default();
        let t0 = Instant::now();
        assert_eq!(p.ended("main", t0).0, 1);
        assert_eq!(p.ended("main", t0).0, 2);
        assert_eq!(p.ended("update", t0).0, 1);
    }

    #[test]
    fn stopped_page_carries_the_page() {
        let page = tauri::Url::parse("https://rd.example.com/m/mbp?x=1#y").unwrap();
        let stopped = stopped_page(&page).unwrap();
        assert!(crate::config::is_app_page(&stopped));
        assert_eq!(stopped.path(), "/stopped.html");
        let url = stopped
            .query_pairs()
            .find(|(k, _)| k == "url")
            .map(|(_, v)| v.into_owned());
        assert_eq!(url.as_deref(), Some(page.as_str()));
        assert_eq!(describe(&page), "https://rd.example.com/m/mbp");
        let local = tauri::Url::parse("http://127.0.0.1:18580/").unwrap();
        assert_eq!(describe(&local), "http://127.0.0.1:18580/");
        assert_eq!(describe(&stopped), "tauri://localhost/stopped.html");
    }
}
