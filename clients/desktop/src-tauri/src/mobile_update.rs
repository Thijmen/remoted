//! The Android APK updater's platform-neutral parts: the manifest, the version code, and the
//! banner injected into the loaded page. `src/android.rs` adds the HTTPS download and the
//! Kotlin calls; keeping this here means it is compiled and tested on every host, not only on
//! an Android build.

use semver::Version;

/// Where the Android update manifest lives: an asset of the `updates` pre-release, like
/// `desktop.json` and `agent.json` (docs/android.md).
pub const MANIFEST_URL: &str =
    "https://github.com/Thijmen/remoted/releases/download/updates/android.json";

/// What the app knows about a newer APK.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Available,
    Downloading,
    Failed,
    Installing,
    Dismissed,
}

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// The part of the manifest the app uses. `platforms` keeps the shape of the other update
/// manifests (`tools/release/publish-manifest.sh` checks for it).
#[derive(serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    pub version_code: u64,
    pub platforms: std::collections::HashMap<String, Platform>,
}

#[derive(serde::Deserialize)]
pub struct Platform {
    pub url: String,
    pub sha256: String,
}

/// Android's `versionCode` as Tauri derives it from the `rd-android` package version
/// (docs/android.md). Keep the two in step: the manifest's code is compared with this one.
pub fn version_code(v: &Version) -> u64 {
    v.major * 1_000_000 + v.minor * 1_000 + v.patch
}

/// The release to offer, if this manifest is newer than `current` and its download is https.
pub fn release_for(manifest: &Manifest, current: &Version) -> Option<Release> {
    if manifest.version_code <= version_code(current) {
        return None;
    }
    let platform = manifest.platforms.get("android-universal")?;
    if !platform.url.starts_with("https://") {
        return None;
    }
    Some(Release {
        version: manifest.version.clone(),
        url: platform.url.clone(),
        sha256: platform.sha256.clone(),
    })
}

/// What the banner shows, as JSON for `__rdAndroidUpdate` (see [`SCRIPT`]).
#[derive(serde::Serialize)]
pub struct Banner {
    pub show: bool,
    pub title: String,
    pub detail: String,
    pub busy: bool,
}

impl Banner {
    pub fn hidden() -> Self {
        Banner {
            show: false,
            title: String::new(),
            detail: String::new(),
            busy: false,
        }
    }
}

/// The banner for a phase, or a hidden one for `Idle`/`Dismissed`.
pub fn banner(
    phase: Phase,
    release: Option<&Release>,
    current: &Version,
    progress: Option<f64>,
    error: Option<&str>,
) -> Banner {
    let Some(version) = release.map(|r| r.version.as_str()) else {
        return Banner::hidden();
    };
    let (title, detail, busy) = match phase {
        Phase::Available => (
            format!("rd {version} is available"),
            format!("You have {current}. Tap Update to install it."),
            false,
        ),
        Phase::Downloading => {
            let pct = progress
                .map(|p| format!(" {}", (p * 100.0).round() as i64))
                .unwrap_or_default();
            (
                format!("Downloading rd {version}…{pct}%"),
                String::new(),
                true,
            )
        }
        Phase::Installing => (
            "Opening the installer…".to_string(),
            "Confirm the update in Android's prompt.".to_string(),
            true,
        ),
        Phase::Failed => (
            "The update did not install".to_string(),
            error.unwrap_or_default().to_string(),
            false,
        ),
        Phase::Idle | Phase::Dismissed => return Banner::hidden(),
    };
    Banner {
        show: true,
        title,
        detail,
        busy,
    }
}

/// The banner, and `window.__rdAndroidUpdate(state)` for later progress updates. It lives in the
/// page's DOM with inline styles (the gateway's CSS is not ours), is never HTML (textContent
/// only), and calls the two-command bridge that the page already has through Tauri's IPC.
pub const SCRIPT: &str = r#"
(() => {
  const ID = 'rd-android-update';
  const css = (el, s) => el.setAttribute('style', s);
  const button = (label, kind, action) => {
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.dataset.rd = kind;
    css(b, 'appearance:none;border:0;border-radius:10px;padding:10px 14px;font:600 14px system-ui,sans-serif;min-height:44px;' +
      (kind === 'update' ? 'background:#e8edf7;color:#0b0d12;' : 'background:transparent;color:#f2f4f8;border:1px solid rgba(255,255,255,.22);'));
    b.onclick = () => {
      const t = window.__TAURI_INTERNALS__;
      if (!t) return;
      if (action === 'later') {
        const el = document.getElementById(ID);
        if (el) el.remove();
      } else {
        b.disabled = true;
      }
      Promise.resolve(t.invoke('rd_android_update', { action })).catch((e) => {
        const el = document.getElementById(ID);
        if (el) {
          const detail = el.querySelector('[data-rd="detail"]');
          if (detail) detail.textContent = String(e);
        }
      });
    };
    return b;
  };
  window.__rdAndroidUpdate = (s) => {
    if (!s || s.show === false) {
      const old = document.getElementById(ID);
      if (old) old.remove();
      return;
    }
    let el = document.getElementById(ID);
    if (!el) {
      if (!document.body) return;
      el = document.createElement('div');
      el.id = ID;
      css(el, 'position:fixed;left:12px;right:12px;bottom:calc(12px + env(safe-area-inset-bottom, 0px));' +
        'z-index:2147483647;display:flex;gap:12px;align-items:center;padding:12px 14px;border-radius:14px;' +
        'background:#16181d;color:#f2f4f8;font:14px/1.35 system-ui,sans-serif;' +
        'box-shadow:0 10px 30px rgba(0,0,0,.45);border:1px solid rgba(255,255,255,.14);max-width:640px;margin:0 auto;');
      const text = document.createElement('div');
      css(text, 'flex:1;min-width:0;');
      const title = document.createElement('div');
      title.dataset.rd = 'title';
      css(title, 'font-weight:600;');
      const detail = document.createElement('div');
      detail.dataset.rd = 'detail';
      css(detail, 'opacity:.75;margin-top:2px;overflow-wrap:anywhere;');
      text.append(title, detail);
      el.append(text, button('Update', 'update', 'install'), button('Later', 'later', 'later'));
      document.body.append(el);
    }
    const title = el.querySelector('[data-rd="title"]');
    const detail = el.querySelector('[data-rd="detail"]');
    const update = el.querySelector('[data-rd="update"]');
    const later = el.querySelector('[data-rd="later"]');
    if (title) title.textContent = s.title;
    if (detail) {
      detail.textContent = s.detail || '';
      css(detail, 'opacity:.75;margin-top:2px;overflow-wrap:anywhere;display:' + (s.detail ? '' : 'none'));
    }
    if (update) {
      update.disabled = !!s.busy;
      css(update, 'appearance:none;border:0;border-radius:10px;padding:10px 14px;font:600 14px system-ui,sans-serif;min-height:44px;background:#e8edf7;color:#0b0d12;' +
        (s.busy ? 'opacity:.5;' : ''));
    }
    if (later) later.disabled = !!s.busy;
  };
  window.__rdAndroidUpdate(__RD_STATE__);
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> Release {
        Release {
            version: "0.2.0".into(),
            url: "https://example.com/a.apk".into(),
            sha256: "00".into(),
        }
    }

    #[test]
    fn version_codes_match_tauris_derivation() {
        assert_eq!(version_code(&Version::parse("0.2.1").unwrap()), 2_001);
        assert_eq!(version_code(&Version::parse("1.7.1").unwrap()), 1_007_001);
        assert_eq!(
            version_code(&Version::parse("12.34.56").unwrap()),
            12_034_056
        );
    }

    #[test]
    fn the_manifest_parses_and_offers_only_newer_https_releases() {
        let current = Version::parse("0.1.0").unwrap();
        let json = r#"{
            "version": "0.2.0",
            "version_code": 2000,
            "platforms": { "android-universal": { "url": "https://example.com/a.apk", "sha256": "ab" } }
        }"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        let found = release_for(&manifest, &current).expect("an update");
        assert_eq!(found.version, "0.2.0");
        assert_eq!(found.sha256, "ab");
        // Same or older: nothing.
        assert!(release_for(&manifest, &Version::parse("0.2.0").unwrap()).is_none());
        // Plain http is refused.
        let plain = json.replace("https://", "http://");
        let manifest: Manifest = serde_json::from_str(&plain).unwrap();
        assert!(release_for(&manifest, &current).is_none());
        // No APK for this platform.
        let empty = json.replace("android-universal", "something-else");
        let manifest: Manifest = serde_json::from_str(&empty).unwrap();
        assert!(release_for(&manifest, &current).is_none());
    }

    #[test]
    fn the_banner_describes_each_phase() {
        let current = Version::parse("0.1.0").unwrap();
        let found = release();
        let available = banner(Phase::Available, Some(&found), &current, None, None);
        assert!(available.show);
        assert_eq!(available.title, "rd 0.2.0 is available");
        assert!(available.detail.contains("0.1.0"));
        assert!(!available.busy);

        let downloading = banner(Phase::Downloading, Some(&found), &current, Some(0.42), None);
        assert!(downloading.busy);
        assert!(downloading.title.contains("42%"), "{}", downloading.title);

        let failed = banner(Phase::Failed, Some(&found), &current, None, Some("offline"));
        assert!(!failed.busy);
        assert_eq!(failed.detail, "offline");

        let installing = banner(Phase::Installing, Some(&found), &current, None, None);
        assert!(installing.busy);
        assert!(installing.title.contains("installer"));

        assert!(!banner(Phase::Dismissed, Some(&found), &current, None, None).show);
        assert!(!banner(Phase::Idle, None, &current, None, None).show);
    }

    #[test]
    fn the_script_is_text_only_and_calls_the_command() {
        assert!(SCRIPT.contains("rd_android_update"));
        assert!(SCRIPT.contains("textContent"));
        assert!(!SCRIPT.contains("innerHTML"));
        let state = serde_json::to_string(&banner(
            Phase::Available,
            Some(&release()),
            &Version::parse("0.1.0").unwrap(),
            None,
            None,
        ))
        .unwrap();
        assert!(!state.contains('<'));
    }
}
