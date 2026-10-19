//! After an update, the first launch of the new version shows "Updated to X.Y.Z · What's new"
//! in the main window for 20 s. Which version ran last is kept in `state.json` in the app's
//! local data directory (per machine, unlike the roaming settings on Windows).
//!
//! The main window shows a remote page (the gateway, a sign-in page) or the setup page, so the
//! notice is injected with `eval` (which works whatever the page's CSP is): a fixed element with
//! a closed shadow root and inline CSSOM styles, so the page's CSS and scripts do not reach it.
//! **What's new** calls `rd_whats_new` (granted to the gateway origin and the setup page); where
//! that is refused, it opens the release page in the browser instead.

use std::path::Path;
use std::time::Duration;

use semver::Version;

/// The state file, in the app local data directory.
pub const STATE_FILE: &str = "state.json";

/// Records that `current` runs now. Returns the version that ran before if this is the first
/// launch after an update to a newer one.
pub fn record_launch(path: &Path, current: &Version) -> Option<Version> {
    let last = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|state| Some(state.get("last_version")?.as_str()?.to_string()));
    let now = current.to_string();
    if last.as_deref() != Some(now.as_str()) {
        let json = serde_json::json!({ "last_version": now });
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, format!("{json:#}\n")));
        if let Err(e) = written {
            log::warn!("cannot write {}: {e}", path.display());
        }
    }
    was_updated(last.as_deref(), current)
}

/// `last` (the version that ran before) if it is older than `current`. No record (a new
/// install, or a version from before the updater) and downgrades show no notice.
pub fn was_updated(last: Option<&str>, current: &Version) -> Option<Version> {
    let last = Version::parse(last?.trim()).ok()?;
    (last < *current).then_some(last)
}

/// The script that shows the notice for `visible`, unless it is already there.
pub fn script(version: &str, release_url: &str, visible: Duration) -> String {
    let json = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(() => {{
  const id = 'rd-desktop-update-notice';
  if (document.getElementById(id)) return;
  const version = {version}, url = {url}, ms = {ms};
  const el = (tag, css, text) => {{
    const e = document.createElement(tag);
    e.style.cssText = css;
    if (text) e.textContent = text;
    return e;
  }};
  const host = el('div', 'all:initial;position:fixed;right:16px;bottom:16px;z-index:2147483647');
  host.id = id;
  const root = host.attachShadow({{ mode: 'closed' }});
  const box = el('div', 'display:flex;align-items:center;gap:10px;padding:9px 8px 9px 14px;' +
    'border-radius:8px;background:#1e293b;color:#f8fafc;box-shadow:0 6px 24px rgba(0,0,0,.35);' +
    'font:13px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif');
  box.setAttribute('role', 'status');
  const text = el('span', '', 'Updated to ' + version + ' · ');
  const link = el('a', 'color:#93c5fd;text-decoration:underline;cursor:pointer', "What's new");
  link.href = url;
  link.addEventListener('click', (ev) => {{
    ev.preventDefault();
    const browser = () => window.open(url, '_blank');
    const ipc = window.__TAURI_INTERNALS__;
    if (ipc) ipc.invoke('rd_whats_new').catch(browser);
    else browser();
  }});
  text.append(link);
  const close = el('button', 'all:unset;cursor:pointer;padding:0 6px;font-size:17px;line-height:1;opacity:.75', '×');
  close.title = 'Dismiss';
  close.setAttribute('aria-label', 'Dismiss');
  close.addEventListener('click', () => host.remove());
  box.append(text, close);
  root.append(box);
  (document.body || document.documentElement).append(host);
  setTimeout(() => host.remove(), ms);
}})()"#,
        version = json(version),
        url = json(release_url),
        ms = visible.as_millis(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn updated_or_not() {
        assert_eq!(was_updated(Some("1.4.2"), &v("1.5.0")), Some(v("1.4.2")));
        assert_eq!(
            was_updated(Some("1.5.0-beta.1"), &v("1.5.0")),
            Some(v("1.5.0-beta.1"))
        );
        assert_eq!(was_updated(Some("1.5.0"), &v("1.5.0")), None);
        assert_eq!(was_updated(Some("1.6.0"), &v("1.5.0")), None, "downgrade");
        assert_eq!(was_updated(None, &v("1.5.0")), None, "new install");
        assert_eq!(was_updated(Some("garbage"), &v("1.5.0")), None);
    }

    #[test]
    fn launches_are_recorded() {
        let dir =
            std::env::temp_dir().join(format!("rd-desktop-test-state-{}", std::process::id()));
        let path = dir.join("sub").join(STATE_FILE);
        assert_eq!(record_launch(&path, &v("1.5.0")), None, "first launch ever");
        assert_eq!(
            record_launch(&path, &v("1.5.0")),
            None,
            "same version again"
        );
        assert_eq!(record_launch(&path, &v("1.6.1")), Some(v("1.5.0")));
        assert_eq!(
            record_launch(&path, &v("1.6.1")),
            None,
            "only the first launch"
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(record_launch(&path, &v("1.7.0")), None);
        assert_eq!(record_launch(&path, &v("1.8.0")), Some(v("1.7.0")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn script_escapes_its_values() {
        let js = script(
            "1.5.0",
            "https://x/y?a='b'&c=</script>",
            Duration::from_secs(20),
        );
        assert!(js.contains(
            r#"const version = "1.5.0", url = "https://x/y?a='b'&c=</script>", ms = 20000;"#
        ));
        let js = script(
            "1.5.0\"); alert(1); (\"",
            "https://x/",
            Duration::from_secs(1),
        );
        assert!(js.contains(r#""1.5.0\"); alert(1); (\"""#));
    }
}
