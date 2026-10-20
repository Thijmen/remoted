//! Routers for the three listeners: viewer (8080), agent (8081), metrics (9090).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use rd_proto::setup_code::SetupCode;
use serde::Deserialize;
use tracing::info;

use crate::agent::ws_agent;
use crate::assets::{self, Caching};
use crate::auth::{ViewerIdentity, viewer_auth};
use crate::devices::{Device, DeviceError};
use crate::files::{ws_agent_files, ws_files};
use crate::probe;
use crate::registry::UpdateRefusal;
use crate::session::{ws_agent_session, ws_view};
use crate::state::AppState;

const REPORT_MAX_BYTES: usize = 1 << 20;
/// Device requests are a machine id and a name.
const DEVICE_MAX_BYTES: usize = 4 << 10;
/// An update request is a version number.
const UPDATE_MAX_BYTES: usize = 1 << 10;

/// `connect-src` also allows the Tauri v2 IPC endpoints (`ipc:` on macOS/Linux, `http://ipc.localhost` on
/// Windows) so the page can call the desktop app (docs/desktop-client.md); browsers have no such endpoints.
const CSP: &str = "default-src 'self'; connect-src 'self' ipc: http://ipc.localhost; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";

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
        // JSON bodies only (the Json extractor wants Content-Type: application/json): a
        // cross-site page cannot send those without a CORS preflight, which the gateway never
        // answers. DELETE is preflighted anyway.
        .route(
            "/api/devices",
            post(api_add_device).layer(DefaultBodyLimit::max(DEVICE_MAX_BYTES)),
        )
        .route(
            "/api/devices/{machine}/token",
            post(api_replace_token).layer(DefaultBodyLimit::max(DEVICE_MAX_BYTES)),
        )
        .route("/api/devices/{machine}", delete(api_remove_device))
        .route(
            "/api/machines/{machine}/update",
            post(api_update_agent).layer(DefaultBodyLimit::max(UPDATE_MAX_BYTES)),
        )
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
        .route("/ws/files/{machine}", get(ws_files))
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
        .route("/ws/agent/files/{channel}", get(ws_agent_files))
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
    no_store(Json(serde_json::json!({
        "email": viewer.user,
        "uid": viewer.uid,
        "dev": st.cfg.dev,
        "devices": st.devices_available(),
    })))
}

async fn api_machines(State(st): State<Arc<AppState>>) -> Response {
    no_store(Json(st.registry.machines(&st.devices.list())))
}

#[derive(Deserialize)]
struct AddDevice {
    machine: String,
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/devices` `{"machine":"studio","name":"Mac Studio"}` → 201 with the setup code.
async fn api_add_device(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Json(req): Json<AddDevice>,
) -> Response {
    let Some(gateway) = st.agent_url.clone() else {
        return device_error(&DeviceError::Disabled);
    };
    let machine = req.machine.trim();
    match st
        .devices
        .add(machine, req.name.as_deref(), &viewer.user, |m| {
            st.agent_auth.provisioned(m)
        }) {
        Ok((device, token)) => {
            info!(target: "audit", event = "device_add", user = viewer.user, ip = %viewer.ip, machine, "device_add");
            issued(StatusCode::CREATED, &device, gateway, token)
        }
        Err(e) => device_error(&e),
    }
}

/// `POST /api/devices/{machine}/token` (body `{}`): a new token; the old one stops working and
/// a connected agent is disconnected.
async fn api_replace_token(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
    Json(_): Json<serde_json::Value>,
) -> Response {
    let Some(gateway) = st.agent_url.clone() else {
        return device_error(&DeviceError::Disabled);
    };
    match st.devices.replace_token(&machine) {
        Ok((device, token)) => {
            st.registry.revoke(&machine, "token replaced", false);
            info!(target: "audit", event = "device_token", user = viewer.user, ip = %viewer.ip, machine, "device_token");
            issued(StatusCode::OK, &device, gateway, token)
        }
        Err(e) => device_error(&e),
    }
}

/// `DELETE /api/devices/{machine}`: the token stops working, a connected agent is
/// disconnected and the device leaves the portal.
async fn api_remove_device(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
) -> Response {
    match st.devices.remove(&machine) {
        Ok(_) => {
            st.registry.revoke(&machine, "device removed", true);
            info!(target: "audit", event = "device_remove", user = viewer.user, ip = %viewer.ip, machine, "device_remove");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => device_error(&e),
    }
}

#[derive(Deserialize)]
struct UpdateAgent {
    version: String,
}

/// `POST /api/machines/{machine}/update` `{"version":"1.6.0"}`: tell the agent to install the
/// update it reported. 202 once the request is queued; the agent reports progress with
/// `update_status`, which the portal reads from `/api/machines`.
async fn api_update_agent(
    State(st): State<Arc<AppState>>,
    Extension(viewer): Extension<ViewerIdentity>,
    Path(machine): Path<String>,
    Json(req): Json<UpdateAgent>,
) -> Response {
    let version = req.version.trim();
    match st
        .registry
        .request_update(&machine, version, &st.devices.list())
    {
        Ok(from) => {
            st.metrics.agent_updates_requested.inc();
            info!(target: "audit", event = "agent_update", user = viewer.user, ip = %viewer.ip, machine, from, to = version, "agent_update");
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "ok": true })),
            )
                .into_response()
        }
        Err(e) => {
            let status = match e {
                UpdateRefusal::Unknown => StatusCode::NOT_FOUND,
                _ => StatusCode::CONFLICT,
            };
            info!(user = viewer.user, machine, to = version, reason = %e, "agent update refused");
            (status, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
        }
    }
}

/// The one response that carries a device's token (and setup code); never cached.
fn issued(status: StatusCode, device: &Device, gateway: String, token: String) -> Response {
    let setup_code = SetupCode {
        gateway: gateway.clone(),
        token: token.clone(),
    }
    .encode();
    let body = serde_json::json!({
        "machine": device.machine,
        "name": device.name,
        "gateway": gateway,
        "token": token,
        "setup_code": setup_code,
    });
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn device_error(e: &DeviceError) -> Response {
    let status = match e {
        DeviceError::Disabled => StatusCode::SERVICE_UNAVAILABLE,
        DeviceError::InvalidMachine | DeviceError::InvalidName => StatusCode::BAD_REQUEST,
        DeviceError::Exists | DeviceError::Provisioned => StatusCode::CONFLICT,
        DeviceError::NotFound => StatusCode::NOT_FOUND,
        DeviceError::Save(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if let DeviceError::Save(why) = e {
        tracing::error!(error = why, "device store not saved");
    }
    (status, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
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
