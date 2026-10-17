//! Viewer build (`viewer/dist`): embedded at compile time, or read from `--assets-dir`.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../viewer/dist"]
#[allow_missing = true]
struct Embedded;

/// Cache policy for a served file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caching {
    /// Content-hashed Vite output under `/assets/`.
    Immutable,
    /// HTML pages and anything else: always revalidate.
    NoCache,
}

impl Caching {
    fn header(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Caching::Immutable => "public, max-age=31536000, immutable",
            Caching::NoCache => "no-cache",
        })
    }
}

pub struct Asset {
    data: Cow<'static, [u8]>,
    mime: String,
    etag: Option<String>,
}

pub enum Assets {
    Embedded,
    Dir(PathBuf),
}

impl Assets {
    pub fn new(dir: Option<PathBuf>) -> Self {
        dir.map_or(Assets::Embedded, Assets::Dir)
    }

    /// Look up `path` (relative, `/`-separated). Paths that could escape the root are refused.
    pub async fn get(&self, path: &str) -> Option<Asset> {
        if !is_safe_path(path) {
            return None;
        }
        match self {
            Assets::Embedded => Embedded::get(path).map(|file| Asset {
                mime: content_type(path, file.metadata.mimetype()),
                etag: Some(format!("\"{}\"", hex::encode(file.metadata.sha256_hash()))),
                data: file.data,
            }),
            Assets::Dir(root) => {
                let data = tokio::fs::read(root.join(path)).await.ok()?;
                let guess = mime_guess::from_path(path).first_or_octet_stream();
                Some(Asset {
                    mime: content_type(path, guess.essence_str()),
                    etag: None,
                    data: Cow::Owned(data),
                })
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Assets::Embedded => format!("embedded ({} files)", Embedded::iter().count()),
            Assets::Dir(dir) => format!("directory {}", dir.display()),
        }
    }
}

fn content_type(path: &str, mime: &str) -> String {
    let textual = mime.starts_with("text/")
        || matches!(
            mime,
            "application/javascript" | "application/json" | "image/svg+xml"
        );
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript; charset=utf-8".to_owned()
    } else if textual {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_owned()
    }
}

fn is_safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Build the response for a found asset (honouring `If-None-Match`).
pub fn respond(asset: Asset, caching: Caching, request: &HeaderMap) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&asset.mime) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(header::CACHE_CONTROL, caching.header());
    if let Some(etag) = asset
        .etag
        .as_deref()
        .and_then(|e| HeaderValue::from_str(e).ok())
    {
        let fresh = request
            .get(header::IF_NONE_MATCH)
            .is_some_and(|v| v.as_bytes() == etag.as_bytes());
        headers.insert(header::ETAG, etag);
        if fresh {
            return (StatusCode::NOT_MODIFIED, headers).into_response();
        }
    }
    let body = match asset.data {
        Cow::Borrowed(data) => Body::from(data),
        Cow::Owned(data) => Body::from(data),
    };
    (headers, body).into_response()
}

/// Shown when a page is missing from the viewer build (e.g. the viewer has not been built).
pub fn missing_page(page: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>rd</title>\
         <body style=\"font:16px system-ui;margin:3em\"><h1>rd gateway</h1>\
         <p>The viewer build does not contain <code>{page}</code>. Build the viewer \
         (<code>cd viewer &amp;&amp; bun run build</code>) or start the gateway with \
         <code>--assets-dir</code>.</p></body>"
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_safety() {
        assert!(is_safe_path("index.html"));
        assert!(is_safe_path("assets/app-1234.js"));
        assert!(!is_safe_path(""));
        assert!(!is_safe_path("../secret"));
        assert!(!is_safe_path("assets/../../x"));
        assert!(!is_safe_path("/etc/passwd"));
        assert!(!is_safe_path("./index.html"));
        assert!(!is_safe_path("a\\b"));
    }

    #[test]
    fn content_types() {
        assert_eq!(
            content_type("a.js", "application/javascript"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("a.html", "text/html"),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type("a.wasm", "application/wasm"),
            "application/wasm"
        );
    }

    #[tokio::test]
    async fn serves_from_dir() {
        let dir = std::env::temp_dir().join(format!("rd-gw-assets-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/x.css"), "body{}").unwrap();
        let assets = Assets::new(Some(dir.clone()));
        let a = assets.get("assets/x.css").await.unwrap();
        assert_eq!(a.mime, "text/css; charset=utf-8");
        assert_eq!(&*a.data, b"body{}");
        assert!(assets.get("assets/missing.css").await.is_none());
        assert!(assets.get("../x.css").await.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
