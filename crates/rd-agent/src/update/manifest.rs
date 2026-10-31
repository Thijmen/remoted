//! The update manifest, `agent.json` (docs/updates.md, "Update manifests"): Tauri's static
//! update JSON plus the release notes of every version.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use rd_proto::control::AvailableUpdate;
use serde::Deserialize;

use super::version::Version;

/// Where the manifest lives; `RD_AGENT_UPDATE_URL` replaces it.
pub const DEFAULT_URL: &str =
    "https://github.com/Thijmen/remoted/releases/download/updates/agent.json";
pub const URL_ENV: &str = "RD_AGENT_UPDATE_URL";
/// `available.notes` stays below this, so `update_status` fits in a control message (64 KiB).
pub const NOTES_MAX: usize = 16 * 1024;

/// The manifest's key for this build's platform, as Tauri names them.
pub const PLATFORM: &str = if cfg!(windows) {
    "windows-x86_64"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "darwin-aarch64"
} else if cfg!(target_os = "macos") {
    "darwin-x86_64"
} else {
    "unsupported"
};

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Notes of `version` alone (Tauri's field); `releases` has every version's.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformFile>,
    /// Newest first.
    #[serde(default)]
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformFile {
    pub url: String,
    /// Base64 of the minisign signature file (what `tauri signer sign` writes).
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    /// The release page.
    #[serde(default)]
    pub url: String,
}

impl Manifest {
    pub fn parse(json: &[u8]) -> anyhow::Result<Manifest> {
        let manifest: Manifest = serde_json::from_slice(json).context("invalid update manifest")?;
        manifest
            .version
            .parse::<Version>()
            .context("invalid version in the update manifest")?;
        Ok(manifest)
    }

    pub fn latest(&self) -> Version {
        // Checked in `parse`.
        self.version.parse().expect("manifest version")
    }

    /// What the portal shows when the manifest has a newer version than `running`.
    pub fn newer_than(&self, running: &Version) -> Option<AvailableUpdate> {
        let latest = self.latest();
        (latest > *running).then(|| AvailableUpdate {
            version: self.version.clone(),
            notes: self.notes_since(running),
            url: self
                .releases
                .iter()
                .find(|r| r.version == self.version)
                .map(|r| r.url.clone())
                .unwrap_or_default(),
        })
    }

    /// The file for this platform.
    pub fn file(&self) -> anyhow::Result<&PlatformFile> {
        match self.platforms.get(PLATFORM) {
            Some(file) => Ok(file),
            None => bail!("version {} has no download for {PLATFORM}", self.version),
        }
    }

    /// The notes of every version newer than `running` (up to the manifest's), newest first,
    /// each under a `## X.Y.Z` heading. At most [`NOTES_MAX`] bytes, cut between versions;
    /// only a newest version whose notes alone are longer is cut inside, at a line end.
    pub fn notes_since(&self, running: &Version) -> String {
        let latest = self.latest();
        let mut newer: Vec<(Version, &str)> = self
            .releases
            .iter()
            .filter_map(|r| Some((r.version.parse::<Version>().ok()?, r.notes.as_str())))
            .filter(|(v, _)| v > running && *v <= latest)
            .collect();
        newer.sort_by(|a, b| b.0.cmp(&a.0));
        newer.dedup_by(|a, b| a.0 == b.0);
        if newer.is_empty() {
            newer.push((latest, self.notes.as_str()));
        }
        let mut out = String::new();
        for (version, notes) in newer {
            let section = format!("## {version}\n\n{}", notes.trim());
            let sep = if out.is_empty() { "" } else { "\n\n" };
            if out.len() + sep.len() + section.len() > NOTES_MAX {
                if out.is_empty() {
                    out = cut_at_line(&section, NOTES_MAX);
                }
                break;
            }
            out.push_str(sep);
            out.push_str(&section);
        }
        out
    }
}

/// `s` cut to at most `max` bytes, at the end of a line when there is one.
fn cut_at_line(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &s[..end];
    cut.rfind('\n').map_or(cut, |i| &cut[..i]).to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn manifest(releases: serde_json::Value) -> Manifest {
        let json = json!({
            "version": "1.6.0",
            "notes": "Only the latest.",
            "pub_date": "2026-10-20T12:00:00Z",
            "platforms": {
                "windows-x86_64": {"url": "https://example.com/setup.exe", "signature": "c2ln"},
                "darwin-aarch64": {"url": "https://example.com/agent.zip", "signature": "c2ln"},
                "darwin-x86_64": {"url": "https://example.com/agent.zip", "signature": "c2ln"},
            },
            "releases": releases,
        });
        Manifest::parse(json.to_string().as_bytes()).unwrap()
    }

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn newer_versions_with_their_notes() {
        let m = manifest(json!([
            {"version": "1.6.0", "notes": "- Six.", "url": "https://example.com/agent-v1.6.0"},
            {"version": "1.5.1", "notes": "- Five one.\n", "url": "u"},
            {"version": "1.5.0", "notes": "- Five.", "url": "u"},
            {"version": "1.4.2", "notes": "- Old.", "url": "u"},
        ]));
        let available = m.newer_than(&v("1.5.0")).unwrap();
        assert_eq!(available.version, "1.6.0");
        assert_eq!(available.url, "https://example.com/agent-v1.6.0");
        assert_eq!(
            available.notes,
            "## 1.6.0\n\n- Six.\n\n## 1.5.1\n\n- Five one."
        );
        assert!(m.newer_than(&v("1.6.0")).is_none());
        assert!(m.newer_than(&v("1.7.0")).is_none());
        // A pre-release of the latest version is older than it.
        assert!(m.newer_than(&v("1.6.0-rc.1")).is_some());
        // This build's download, where there are release builds (none for Linux yet).
        assert_eq!(m.file().is_ok(), cfg!(any(target_os = "macos", windows)));
    }

    #[test]
    fn notes_without_releases_are_the_latest_version_s() {
        let m = manifest(json!([]));
        assert_eq!(m.notes_since(&v("1.0.0")), "## 1.6.0\n\nOnly the latest.");
        assert_eq!(m.newer_than(&v("1.0.0")).unwrap().url, "");
    }

    #[test]
    fn notes_are_capped_between_versions() {
        let long = "- x\n".repeat(NOTES_MAX / 10);
        let m = manifest(json!([
            {"version": "1.6.0", "notes": long},
            {"version": "1.5.0", "notes": long},
            {"version": "1.4.0", "notes": "- z\n".repeat(1000)},
        ]));
        let notes = m.notes_since(&v("1.0.0"));
        assert!(notes.len() <= NOTES_MAX);
        assert!(notes.starts_with("## 1.6.0\n\n- x\n"));
        assert!(notes.contains("## 1.5.0"));
        assert!(!notes.contains("## 1.4.0"), "{}", notes.len());

        // One version longer than the cap alone: cut at a line end.
        let huge = "- yyyyyyy\n".repeat(NOTES_MAX / 5);
        let m = manifest(json!([{"version": "1.6.0", "notes": huge}]));
        let notes = m.notes_since(&v("1.0.0"));
        assert!(notes.len() <= NOTES_MAX);
        assert!(
            notes.ends_with("- yyyyyyy"),
            "{:?}",
            &notes[notes.len() - 20..]
        );
    }

    #[test]
    fn rejects_bad_manifests() {
        assert!(Manifest::parse(b"{}").is_err());
        assert!(Manifest::parse(br#"{"version":"latest"}"#).is_err());
        let m = Manifest::parse(br#"{"version":"2.0.0"}"#).unwrap();
        assert!(m.file().is_err());
        assert_eq!(cut_at_line("ab\ncd", 4), "ab");
        assert_eq!(cut_at_line("éé", 3), "é");
    }
}
