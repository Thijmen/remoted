//! Which gateway to load, and which URLs may load inside the window.

use tauri::Url;

/// The production gateway. Overridden by `--url <URL>` or `RD_URL` (local testing).
pub const DEFAULT_URL: &str = "https://rd.example.com";

/// Login redirects go to Authentik on a sibling host, so this domain may load in the window too.
/// Only the gateway origin gets the `rd_set_grab` capability.
const TRUSTED_DOMAIN: &str = "example.com";

/// The URL the window opens: `--url <URL>` / `--url=<URL>`, else `RD_URL`, else [`DEFAULT_URL`].
pub fn gateway_url(
    mut args: impl Iterator<Item = String>,
    env: Option<String>,
) -> Result<Url, String> {
    let mut from_args = None;
    while let Some(arg) = args.next() {
        if arg == "--url" {
            from_args = Some(args.next().ok_or("--url needs a value")?);
        } else if let Some(v) = arg.strip_prefix("--url=") {
            from_args = Some(v.to_string());
        }
    }
    let raw = from_args
        .or(env.filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let url = Url::parse(raw.trim()).map_err(|e| format!("invalid gateway URL {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("gateway URL must be http(s)://host…, got {raw:?}"));
    }
    Ok(url)
}

/// `scheme://host[:port]` of a URL (what a capability's `remote.urls` entry matches).
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// May `url` load inside the window? Everything else opens in the system browser.
pub fn allowed_in_window(gateway: &Url, url: &Url) -> bool {
    if url.origin() == gateway.origin() {
        return true;
    }
    match url.scheme() {
        "about" => matches!(url.path(), "blank" | "srcdoc"),
        "https" => url.port().is_none() && url.host_str().is_some_and(is_trusted_host),
        _ => false,
    }
}

fn is_trusted_host(host: &str) -> bool {
    host == TRUSTED_DOMAIN
        || host
            .strip_suffix(TRUSTED_DOMAIN)
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
    fn url_precedence() {
        let def = gateway_url(args(&["rd-desktop"]), None).unwrap();
        assert_eq!(def.as_str(), "https://rd.example.com/");
        let env = gateway_url(args(&["x"]), Some("http://127.0.0.1:18580".into())).unwrap();
        assert_eq!(origin(&env), "http://127.0.0.1:18580");
        let arg = gateway_url(
            args(&["x", "--url", "http://localhost:1/"]),
            Some("http://127.0.0.1:2".into()),
        )
        .unwrap();
        assert_eq!(origin(&arg), "http://localhost:1");
        let eq = gateway_url(args(&["x", "--url=http://h:3/p?q"]), None).unwrap();
        assert_eq!(eq.as_str(), "http://h:3/p?q");
        assert!(gateway_url(args(&["x"]), Some("   ".into())).is_ok());
        assert!(gateway_url(args(&["x", "--url"]), None).is_err());
        assert!(gateway_url(args(&["x", "--url", "file:///etc/passwd"]), None).is_err());
        assert!(gateway_url(args(&["x", "--url", "nonsense"]), None).is_err());
    }

    #[test]
    fn navigation_policy() {
        let gw = u(DEFAULT_URL);
        assert!(allowed_in_window(
            &gw,
            &u("https://rd.example.com/m/mbp?x=1")
        ));
        assert!(allowed_in_window(
            &gw,
            &u("https://authentik.example.com/flows/x")
        ));
        assert!(allowed_in_window(&gw, &u("https://example.com/")));
        assert!(allowed_in_window(&gw, &u("about:blank")));
        assert!(!allowed_in_window(
            &gw,
            &u("http://authentik.example.com/")
        ));
        assert!(!allowed_in_window(
            &gw,
            &u("https://authentik.example.com:8443/")
        ));
        assert!(!allowed_in_window(
            &gw,
            &u("https://evilexample.com/")
        ));
        assert!(!allowed_in_window(
            &gw,
            &u("https://example.com.evil.com/")
        ));
        assert!(!allowed_in_window(&gw, &u("https://github.com/")));
        assert!(!allowed_in_window(&gw, &u("file:///C:/Windows")));

        let dev = u("http://127.0.0.1:18580/");
        assert!(allowed_in_window(&dev, &u("http://127.0.0.1:18580/portal")));
        assert!(!allowed_in_window(&dev, &u("http://127.0.0.1:18581/")));
        assert!(!allowed_in_window(&dev, &u("http://localhost:18580/")));

        assert!(openable_externally(&u("https://github.com/")));
        assert!(!openable_externally(&u("file:///C:/Windows")));
        assert!(!openable_externally(&u("ms-settings:privacy")));
    }
}
