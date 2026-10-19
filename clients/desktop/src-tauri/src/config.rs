//! Which gateway to load, and which URLs may load inside the window.
//!
//! The gateway comes from `--url <URL>` / `RD_URL` (not saved; local testing), else from the
//! settings file the setup page writes (`settings.json` in the app config directory). Without
//! either, or with `--setup`, the window opens the bundled setup page instead.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::Url;

/// What the window may load and grant `rd_set_grab` to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The gateway (viewer) URL, e.g. `https://rd.example.com`.
    pub gateway_url: Url,
    /// Hosts under this domain may load in the window too (e.g. `example.com` for a login page
    /// at `auth.example.com`). Only the gateway origin gets the `rd_set_grab` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in_domain: Option<String>,
}

/// The parsed command line and environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `--url` / `RD_URL`: use this gateway for this run only.
    pub url_override: Option<Url>,
    /// `--setup`: show the setup page even if a gateway is saved.
    pub setup: bool,
}

/// `--url <URL>` / `--url=<URL>` (wins over `RD_URL`), and `--setup`.
pub fn parse_args(
    mut args: impl Iterator<Item = String>,
    env: Option<String>,
) -> Result<Args, String> {
    let mut from_args = None;
    let mut setup = false;
    while let Some(arg) = args.next() {
        if arg == "--url" {
            from_args = Some(args.next().ok_or("--url needs a value")?);
        } else if let Some(v) = arg.strip_prefix("--url=") {
            from_args = Some(v.to_string());
        } else if arg == "--setup" {
            setup = true;
        }
    }
    let url_override = from_args
        .or(env.filter(|v| !v.trim().is_empty()))
        .map(|raw| override_url(&raw))
        .transpose()?;
    Ok(Args {
        url_override,
        setup,
    })
}

/// A `--url` / `RD_URL` value: any http(s) URL (local testing may use plain http).
fn override_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("invalid gateway URL {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("gateway URL must be http(s)://host…, got {raw:?}"));
    }
    Ok(url)
}

/// Checks what the setup page submitted. `https://` is added when no scheme is given; plain
/// http is only accepted for this machine (the viewer needs a secure context anyway).
pub fn validate(gateway_url: &str, sign_in_domain: Option<&str>) -> Result<Settings, String> {
    let raw = gateway_url.trim();
    if raw.is_empty() {
        return Err("enter the gateway URL".into());
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let url = Url::parse(&with_scheme).map_err(|e| format!("not a valid URL: {e}"))?;
    let Some(host) = url.host_str() else {
        return Err("the URL needs a host name".into());
    };
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(host) => {}
        "http" => return Err("use https:// (plain http only works for localhost)".into()),
        s => return Err(format!("use https://, not {s}://")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the URL must not contain a user name or password".into());
    }

    let sign_in_domain = match sign_in_domain.map(|d| d.trim().trim_end_matches('.')) {
        None | Some("") => None,
        Some(d) => {
            let d = d.to_ascii_lowercase();
            if !is_domain(&d) {
                return Err(format!(
                    "{d:?} is not a domain name like example.com (no scheme, path or port)"
                ));
            }
            Some(d)
        }
    };
    Ok(Settings {
        gateway_url: url,
        sign_in_domain,
    })
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// At least two labels of `[a-z0-9-]`, so a bare top-level domain like `com` is refused.
fn is_domain(d: &str) -> bool {
    let labels: Vec<&str> = d.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Reads the settings file; `None` when it does not exist or cannot be used.
pub fn load(path: &Path) -> Option<Settings> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("cannot read {}: {e}", path.display());
            return None;
        }
    };
    let parsed: Settings = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("ignoring {}: {e}", path.display());
            return None;
        }
    };
    // Same rules as the setup page, so a hand-edited file cannot loosen them.
    validate(
        parsed.gateway_url.as_str(),
        parsed.sign_in_domain.as_deref(),
    )
    .inspect_err(|e| log::warn!("ignoring {}: {e}", path.display()))
    .ok()
}

pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, json + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// `scheme://host[:port]` of a URL (what a capability's `remote.urls` entry matches).
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The app's own pages (the setup page): `tauri://localhost` on macOS,
/// `http(s)://tauri.localhost` on Windows.
pub fn is_app_page(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost") && url.port().is_none(),
        _ => false,
    }
}

/// May `url` load inside the window? Everything else opens in the system browser.
pub fn allowed_in_window(settings: Option<&Settings>, url: &Url) -> bool {
    if is_app_page(url) {
        return true;
    }
    match url.scheme() {
        "about" => return matches!(url.path(), "blank" | "srcdoc"),
        "https" | "http" => {}
        _ => return false,
    }
    let Some(settings) = settings else {
        return false;
    };
    if url.origin() == settings.gateway_url.origin() {
        return true;
    }
    url.scheme() == "https"
        && url.port().is_none()
        && settings
            .sign_in_domain
            .as_deref()
            .zip(url.host_str())
            .is_some_and(|(domain, host)| is_under(host, domain))
}

/// `host` is `domain` or a subdomain of it.
fn is_under(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1)
}

/// May a URL that is not allowed in the window be handed to the system browser?
pub fn openable_externally(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "mailto")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> impl Iterator<Item = String> {
        v.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn arg_parsing() {
        assert_eq!(parse_args(args(&["x"]), None).unwrap(), Args::default());
        let env = parse_args(args(&["x"]), Some("http://127.0.0.1:18580".into())).unwrap();
        assert_eq!(origin(&env.url_override.unwrap()), "http://127.0.0.1:18580");
        let arg = parse_args(
            args(&["x", "--url", "http://localhost:1/"]),
            Some("http://127.0.0.1:2".into()),
        )
        .unwrap();
        assert_eq!(origin(&arg.url_override.unwrap()), "http://localhost:1");
        let eq = parse_args(args(&["x", "--url=http://h:3/p?q"]), None).unwrap();
        assert_eq!(eq.url_override.unwrap().as_str(), "http://h:3/p?q");
        assert!(parse_args(args(&["x", "--setup"]), None).unwrap().setup);
        assert_eq!(
            parse_args(args(&["x"]), Some("   ".into())).unwrap(),
            Args::default()
        );
        assert!(parse_args(args(&["x", "--url"]), None).is_err());
        assert!(parse_args(args(&["x", "--url", "file:///etc/passwd"]), None).is_err());
        assert!(parse_args(args(&["x", "--url", "nonsense"]), None).is_err());
    }

    #[test]
    fn setup_validation() {
        let s = validate(" rd.example.com ", Some(" Example.COM. ")).unwrap();
        assert_eq!(s.gateway_url.as_str(), "https://rd.example.com/");
        assert_eq!(s.sign_in_domain.as_deref(), Some("example.com"));
        let s = validate("https://rd.example.com/m/mbp", Some("")).unwrap();
        assert_eq!(s.gateway_url.path(), "/m/mbp");
        assert_eq!(s.sign_in_domain, None);
        assert!(validate("http://localhost:18580", None).is_ok());
        assert!(validate("http://127.0.0.1:18580", None).is_ok());

        assert!(validate("", None).is_err());
        assert!(validate("http://rd.example.com", None).is_err());
        assert!(validate("ftp://rd.example.com", None).is_err());
        assert!(validate("https://user:pw@rd.example.com", None).is_err());
        assert!(validate("rd.example.com", Some("com")).is_err());
        assert!(validate("rd.example.com", Some("https://example.com")).is_err());
        assert!(validate("rd.example.com", Some("example.com:443")).is_err());
        assert!(validate("rd.example.com", Some("*.example.com")).is_err());
    }

    #[test]
    fn settings_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("rd-desktop-test-{}", std::process::id()));
        let path = dir.join("sub").join("settings.json");
        assert_eq!(load(&path), None);
        let s = validate("rd.example.com", Some("example.com")).unwrap();
        save(&path, &s).unwrap();
        assert_eq!(load(&path), Some(s));
        std::fs::write(&path, r#"{"gateway_url":"http://rd.example.com/"}"#).unwrap();
        assert_eq!(load(&path), None, "plain http is refused in the file too");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn navigation_policy() {
        let gw = validate("https://rd.example.com", Some("example.com")).unwrap();
        let gw = Some(&gw);
        assert!(allowed_in_window(
            gw,
            &u("https://rd.example.com/m/mbp?x=1")
        ));
        assert!(allowed_in_window(
            gw,
            &u("https://auth.example.com/flows/x")
        ));
        assert!(allowed_in_window(gw, &u("https://example.com/")));
        assert!(allowed_in_window(gw, &u("about:blank")));
        assert!(!allowed_in_window(gw, &u("http://auth.example.com/")));
        assert!(!allowed_in_window(gw, &u("https://auth.example.com:8443/")));
        assert!(!allowed_in_window(gw, &u("https://evilexample.com/")));
        assert!(!allowed_in_window(gw, &u("https://example.com.evil.com/")));
        assert!(!allowed_in_window(gw, &u("https://github.com/")));
        assert!(!allowed_in_window(gw, &u("file:///C:/Windows")));

        let no_domain = validate("https://rd.example.com", None).unwrap();
        assert!(!allowed_in_window(
            Some(&no_domain),
            &u("https://auth.example.com/")
        ));

        let dev = Settings {
            gateway_url: u("http://127.0.0.1:18580/"),
            sign_in_domain: None,
        };
        let dev = Some(&dev);
        assert!(allowed_in_window(dev, &u("http://127.0.0.1:18580/portal")));
        assert!(!allowed_in_window(dev, &u("http://127.0.0.1:18581/")));
        assert!(!allowed_in_window(dev, &u("http://localhost:18580/")));

        // The setup page, with or without a gateway.
        for page in ["tauri://localhost/index.html", "http://tauri.localhost/"] {
            assert!(allowed_in_window(None, &u(page)));
            assert!(allowed_in_window(gw, &u(page)));
        }
        assert!(!allowed_in_window(None, &u("http://tauri.localhost:8080/")));
        assert!(!allowed_in_window(None, &u("https://rd.example.com/")));

        assert!(openable_externally(&u("https://github.com/")));
        assert!(!openable_externally(&u("file:///C:/Windows")));
        assert!(!openable_externally(&u("ms-settings:privacy")));
    }
}
