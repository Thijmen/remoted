//! HTTPS for the updater: ureq (blocking) with rustls + ring and the webpki roots, like the
//! gateway connection. Redirects are followed (GitHub sends release assets to another host).
//! Callers run it off the async runtime.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use url::{Host, Url};

use super::manifest::Manifest;

/// The manifest is small; anything bigger is not one.
const MANIFEST_MAX: u64 = 1 << 20;
/// Update files are tens of MB; this only stops a runaway download.
pub const DOWNLOAD_MAX: u64 = 300 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
/// A whole download, at slow-but-working speeds.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new(user_agent: &str) -> Http {
        let config = ureq::Agent::config_builder()
            .user_agent(user_agent)
            .https_only(true)
            .max_redirects(10)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .build();
        Http {
            agent: config.into(),
        }
    }

    pub fn manifest(&self, url: &str) -> anyhow::Result<Manifest> {
        let https_only = check_url(url)?;
        let mut response = self
            .agent
            .get(url)
            .config()
            .https_only(https_only)
            .timeout_global(Some(MANIFEST_TIMEOUT))
            .build()
            .call()
            .with_context(|| format!("fetching {url}"))?;
        let json = response
            .body_mut()
            .with_config()
            .limit(MANIFEST_MAX)
            .read_to_vec()
            .with_context(|| format!("reading {url}"))?;
        Manifest::parse(&json)
    }

    /// Writes `url` to `dest` (replacing it). At most [`DOWNLOAD_MAX`] bytes.
    pub fn download(&self, url: &str, dest: &Path) -> anyhow::Result<u64> {
        let https_only = check_url(url)?;
        let response = self
            .agent
            .get(url)
            .config()
            .https_only(https_only)
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .build()
            .call()
            .with_context(|| format!("downloading {url}"))?;
        let mut body = response.into_body();
        if let Some(len) = body.content_length()
            && len > DOWNLOAD_MAX
        {
            bail!("{url} is {len} bytes, more than an update can be");
        }
        let mut reader = body.with_config().limit(DOWNLOAD_MAX).reader();
        let mut file =
            std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        let written =
            std::io::copy(&mut reader, &mut file).with_context(|| format!("downloading {url}"))?;
        file.flush()?;
        file.sync_all()?;
        Ok(written)
    }
}

/// Update URLs must be HTTPS. Debug builds also take plain HTTP to a loopback address (for
/// tests); returns whether the request must stay on HTTPS.
pub fn check_url(url: &str) -> anyhow::Result<bool> {
    let parsed = Url::parse(url).with_context(|| format!("invalid update URL {url:?}"))?;
    match parsed.scheme() {
        "https" => Ok(true),
        "http" if cfg!(debug_assertions) && loopback(&parsed) => Ok(false),
        _ => bail!("update URLs must use https: {url}"),
    }
}

fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_only() {
        assert!(check_url("https://github.com/x").unwrap());
        assert!(check_url("http://example.com/agent.json").is_err());
        assert!(check_url("ftp://example.com/agent.json").is_err());
        assert!(check_url("not a url").is_err());
        // Tests serve plain HTTP on loopback; release builds never accept it.
        let local = check_url("http://127.0.0.1:8000/agent.json");
        assert_eq!(local.is_ok(), cfg!(debug_assertions));
        assert_eq!(
            check_url("http://[::1]:8000/a").is_ok(),
            cfg!(debug_assertions)
        );
        assert!(check_url("http://127.0.0.1.example.com/a").is_err());
    }
}
