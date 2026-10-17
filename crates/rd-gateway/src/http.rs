//! Routers for the three listeners: viewer (8080), agent (8081), metrics (9090).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::agent::ws_agent;
use crate::assets::{self, Caching};
use crate::auth::{ViewerIdentity, viewer_auth};
use crate::probe;
use crate::session::{ws_agent_session, ws_view};
use crate::state::AppState;

const REPORT_MAX_BYTES: usize = 1 << 20;

const CSP: &str = "default-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";

pub fn viewer_router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|s, h| page(s, h, "index.html")))
        .route("/m/{machine}", get(viewer_page))
        .route("/probe", get(|s, h| page(s, h, "probe.html")))
        .route("/latency-target", get(|s, h| page(s, h, "latency.html")))
        .route("/keytest", get(|s, h| page(s, h, "keytest.html")))
        .route("/assets/{*path}", get(hashed_asset))
        .route("/api/me", get(api_me))
        .route("/api/machines", get(api_machines))
        .route(
            "/api/probe/report",
            post(probe::http_report).layer(DefaultBodyLimit::max(REPORT_MAX_BYTES)),
        )
        .route("/api/probe/down", get(probe::http_down))
        .route(
            "/api/probe/up",
            post(probe::http_up).layer(DefaultBodyLimit::disable()),
        )
        .route("/ws/view/{machine}", get(ws_view))
        .route("/ws/probe", get(probe::ws_probe))
        .fallback(root_file)
        .layer(from_fn_with_state(Arc::clone(&st), viewer_auth))
        .layer(map_response(security_headers))
        .with_state(st)
}

pub fn agent_router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/ws/agent", get(ws_agent))
        .route("/ws/agent/session/{session}", get(ws_agent_session))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(st)
}

pub fn metrics_router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(st)
}

async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        "cross-origin-embedder-policy",
        HeaderValue::from_static("require-corp"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // No framing: a page that embeds the viewer could steer remote input (clickjacking).
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    res
}

async fn page(State(st): State<Arc<AppState>>, headers: HeaderMap, name: &'static str) -> Response {
    match st.assets.get(name).await {
        Some(asset) => assets::respond(asset, Caching::NoCache, &headers),
        None => assets::missing_page(name),
    }
}

/// `/m/{machine}`: the page reads the machine from `location.pathname`.
async fn viewer_page(
    st: State<Arc<AppState>>,
    headers: HeaderMap,
    Path(_machine): Path<String>,
) -> Response {
    page(st, headers, "viewer.html").await
}

async fn hashed_asset(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    match st.assets.get(&format!("assets/{path}")).await {
        Some(asset) => assets::respond(asset, Caching::Immutable, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Other top-level files of the viewer build (favicon, worklets, ...).
async fn root_file(
    State(st): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    match st.assets.get(uri.path().trim_start_matches('/')).await {
        Some(asset) => assets::respond(asset, Caching::NoCache, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_me(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
) -> Response {
    no_store(Json(
        serde_json::json!({ "email": viewer.user, "uid": viewer.uid, "dev": st.cfg.dev }),
    ))
}

async fn api_machines(State(st): State<Arc<AppState>>) -> Response {
    no_store(Json(st.registry.machines()))
}

fn no_store(body: impl IntoResponse) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn metrics(State(st): State<Arc<AppState>>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        st.metrics.render(st.registry.online_count()),
    )
        .into_response()
}
