//! Release notes from the update manifest (docs/updates.md, "Update manifests"): its `releases`
//! array lists every version's notes as Markdown, newest first. The update window shows the
//! versions between two versions, rendered to HTML here.
//!
//! The manifest is not signed (only the files it points to are), so its Markdown is untrusted:
//! raw HTML is shown as text, images are dropped (they would load from anywhere) and only
//! http(s) links stay links. The page also forbids scripts and remote content by CSP.

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use semver::Version;
use serde::Serialize;

/// One entry of the manifest's `releases`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Markdown.
    pub notes: String,
    /// The version's GitHub release page (http(s) only).
    pub url: Option<String>,
}

/// What the update window gets for one version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Shown {
    pub version: String,
    pub html: String,
    pub url: Option<String>,
}

/// The `releases` of a manifest, newest first. Entries without a valid version are skipped;
/// a missing or malformed array gives none (the window then links to the release page only).
pub fn releases(manifest: &serde_json::Value) -> Vec<Release> {
    let Some(list) = manifest.get("releases").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<Release> = list
        .iter()
        .filter_map(|entry| {
            let version = entry.get("version")?.as_str()?;
            let version = Version::parse(version.trim().trim_start_matches('v')).ok()?;
            let notes = entry
                .get("notes")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let url = entry
                .get("url")
                .and_then(|u| u.as_str())
                .filter(|u| is_web_url(u))
                .map(str::to_string);
            Some(Release {
                version,
                notes,
                url,
            })
        })
        .collect();
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out.dedup_by(|a, b| a.version == b.version);
    out
}

/// The versions newer than `after` and, if given, not newer than `upto`, newest first.
pub fn between<'a>(
    releases: &'a [Release],
    after: &Version,
    upto: Option<&Version>,
) -> Vec<&'a Release> {
    releases
        .iter()
        .filter(|r| r.version > *after && upto.is_none_or(|u| r.version <= *u))
        .collect()
}

/// [`between`], rendered for the window.
pub fn shown(releases: &[Release], after: &Version, upto: Option<&Version>) -> Vec<Shown> {
    between(releases, after, upto)
        .into_iter()
        .map(|r| Shown {
            version: r.version.to_string(),
            html: to_html(&r.notes),
            url: r.url.clone(),
        })
        .collect()
}

/// An absolute http(s) URL (what may be opened in the system browser).
pub fn is_web_url(s: &str) -> bool {
    tauri::Url::parse(s)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

/// Markdown to HTML that is safe to insert into the update page: raw HTML becomes text, images
/// become their alt text, links that are not absolute http(s) URLs become their text.
pub fn to_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    // Link and image ends do not say whether their start was kept, so track it.
    let mut kept_links: Vec<bool> = Vec::new();
    let mut in_image = 0usize;
    let events = Parser::new_ext(markdown, options).filter_map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Some(Event::Text(html)),
        Event::Start(Tag::Image { .. }) => {
            in_image += 1;
            None
        }
        Event::End(TagEnd::Image) => {
            in_image = in_image.saturating_sub(1);
            None
        }
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let keep = link_type != LinkType::Email && is_web_url(&dest_url);
            kept_links.push(keep);
            keep.then_some(Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }))
        }
        Event::End(TagEnd::Link) => kept_links
            .pop()
            .unwrap_or(false)
            .then_some(Event::End(TagEnd::Link)),
        // Inside an image only its alt text remains.
        e @ (Event::Text(_) | Event::Code(_)) if in_image > 0 => Some(e),
        _ if in_image > 0 => None,
        e => Some(e),
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn manifest() -> serde_json::Value {
        serde_json::json!({
            "version": "1.6.0",
            "platforms": {},
            "releases": [
                { "version": "1.5.0", "notes": "- five", "url": "https://github.com/x/y/releases/tag/desktop-v1.5.0" },
                { "version": "1.6.0", "notes": "- six", "url": "https://github.com/x/y/releases/tag/desktop-v1.6.0" },
                { "version": "1.4.2", "notes": "- old", "url": "javascript:alert(1)" },
                { "version": "not a version", "notes": "- skipped" },
                { "notes": "- no version" },
                { "version": "v1.10.0", "notes": "- ten" },
                { "version": "1.6.0", "notes": "- duplicate" }
            ]
        })
    }

    #[test]
    fn releases_are_parsed_newest_first() {
        let r = releases(&manifest());
        let versions: Vec<String> = r.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, ["1.10.0", "1.6.0", "1.5.0", "1.4.2"]);
        assert_eq!(r[1].notes, "- six");
        assert_eq!(r[3].url, None, "only http(s) release links");
        assert!(releases(&serde_json::json!({ "version": "1.0.0" })).is_empty());
        assert!(releases(&serde_json::json!({ "releases": "nope" })).is_empty());
    }

    #[test]
    fn versions_between_two_versions() {
        let r = releases(&manifest());
        let pick = |after: &str, upto: Option<&str>| -> Vec<String> {
            let upto = upto.map(v);
            between(&r, &v(after), upto.as_ref())
                .iter()
                .map(|r| r.version.to_string())
                .collect()
        };
        // Update prompt: everything newer than the installed version.
        assert_eq!(pick("1.4.2", None), ["1.10.0", "1.6.0", "1.5.0"]);
        assert_eq!(pick("1.5.0", None), ["1.10.0", "1.6.0"]);
        assert!(pick("1.10.0", None).is_empty());
        // After an update from 1.4.2 to 1.6.0: what came with it, not what is newer still.
        assert_eq!(pick("1.4.2", Some("1.6.0")), ["1.6.0", "1.5.0"]);
        assert!(pick("1.6.0", Some("1.6.0")).is_empty());
        // Pre-releases sort below their release.
        assert!(v("1.6.0-beta.1") < v("1.6.0"));
        assert_eq!(pick("1.6.0-beta.1", Some("1.6.0")), ["1.6.0"]);
    }

    #[test]
    fn markdown_is_rendered() {
        let html = to_html(
            "- **Cmd+`** cycles windows\n- `code` and ~~gone~~\n\nSee [the docs](https://example.com/a?b=1&c=2).",
        );
        assert!(html.contains("<li><strong>Cmd+`</strong> cycles windows</li>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains(r#"<a href="https://example.com/a?b=1&amp;c=2">the docs</a>"#));
        assert!(to_html("<https://x.y/>").contains(r#"<a href="https://x.y/">https://x.y/</a>"#));
    }

    #[test]
    fn markdown_is_sanitized() {
        let html = to_html(
            "<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)> <b>bold</b>\n\n\
             [js](javascript:alert(1)) [rel](/local) [mail](mailto:a@b.c) <a@b.c> \
             ![alt *text*](https://tracker.example/pixel.png) [![badge](https://x/b.png)](https://ok.example/)",
        );
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(!html.contains("href=\"/local\""), "{html}");
        assert!(!html.contains("mailto:"), "{html}");
        assert!(html.contains("js") && html.contains("rel") && html.contains("mail"));
        assert!(html.contains("alt text"), "{html}");
        assert!(!html.contains("tracker.example"), "{html}");
        // A kept link around a dropped image keeps the alt text as its text.
        assert!(
            html.contains(r#"<a href="https://ok.example/">badge</a>"#),
            "{html}"
        );
    }

    #[test]
    fn web_urls() {
        assert!(is_web_url("https://github.com/x"));
        assert!(is_web_url("http://127.0.0.1:8080/x"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("file:///etc/passwd"));
        assert!(!is_web_url("/relative"));
        assert!(!is_web_url("mailto:a@b.c"));
    }
}
