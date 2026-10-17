//! Agent bearer-token auth (with per-IP failure rate limiting) and the viewer auth middleware.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::{Clock as _, DefaultClock};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::config::valid_machine_name;
use crate::state::AppState;
use crate::util::client_ip;

/// Failed agent authentications allowed per client IP per minute.
const AUTH_FAILURES_PER_MINUTE: u32 = 5;
/// Prune limiter state once this many IPs are tracked.
const LIMITER_PRUNE_AT: usize = 1024;
const TOKEN_PREFIX: &str = "rd1";

/// Identity of an authenticated agent connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    /// Machine the token belongs to. `None` only in dev mode for tokens without the
    /// `rd1.<machine>.` form (the machine then comes from `register`).
    pub machine: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRejection {
    Unauthorized,
    RateLimited { retry_after: Duration },
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        match self {
            AuthRejection::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized\n",
            )
                .into_response(),
            AuthRejection::RateLimited { retry_after } => (
                StatusCode::TOO_MANY_REQUESTS,
                [(
                    header::RETRY_AFTER,
                    retry_after.as_secs().max(1).to_string(),
                )],
                "too many failed attempts\n",
            )
                .into_response(),
        }
    }
}

/// Verifies `Authorization: Bearer rd1.<machine>.<secret>` against
/// `{tokens_dir}/<machine>` (hex SHA-256 of the whole token).
pub struct AgentAuth {
    dev: bool,
    tokens_dir: PathBuf,
    failures: DefaultKeyedRateLimiter<IpAddr>,
    clock: DefaultClock,
    /// IPs that exhausted their failure budget, until when. Checked before the token so a
    /// blocked client cannot keep guessing.
    /// Blocked IPs: until when, and whether the block has been logged yet.
    blocked: Mutex<HashMap<IpAddr, (Instant, bool)>>,
}

impl AgentAuth {
    pub fn new(dev: bool, tokens_dir: PathBuf) -> Self {
        let per_minute = NonZeroU32::new(AUTH_FAILURES_PER_MINUTE).expect("non-zero constant");
        AgentAuth {
            dev,
            tokens_dir,
            failures: RateLimiter::keyed(Quota::per_minute(per_minute)),
            clock: DefaultClock::default(),
            blocked: Mutex::new(HashMap::new()),
        }
    }

    pub async fn verify(
        &self,
        headers: &HeaderMap,
        ip: IpAddr,
        path: &str,
    ) -> Result<AgentIdentity, AuthRejection> {
        if let Some((retry_after, first)) = self.blocked_for(ip) {
            // Once per block: a client hammering while blocked must not flood the logs.
            if first {
                warn!(%ip, path, "agent auth rejected: rate limited (further attempts not logged)");
            }
            return Err(AuthRejection::RateLimited { retry_after });
        }
        let token = bearer_token(headers);
        if self.dev {
            return Ok(AgentIdentity {
                machine: token.and_then(|t| machine_of(t).map(str::to_owned)),
            });
        }
        match self.check_token(token).await {
            Ok(machine) => Ok(AgentIdentity {
                machine: Some(machine),
            }),
            Err(reason) => {
                warn!(%ip, path, reason, "agent auth failed");
                Err(self.record_failure(ip))
            }
        }
    }

    async fn check_token(&self, token: Option<&str>) -> Result<String, &'static str> {
        let token = token.ok_or("missing bearer token")?;
        let machine = machine_of(token).ok_or("malformed token")?;
        let path = self.tokens_dir.join(machine);
        let stored = tokio::fs::read_to_string(&path)
            .await
            .map_err(|_| "unknown machine")?;
        let mut expected = [0u8; 32];
        hex::decode_to_slice(stored.trim(), &mut expected).map_err(|_| "corrupt token file")?;
        let actual = Sha256::digest(token.as_bytes());
        if bool::from(actual.as_slice().ct_eq(&expected)) {
            Ok(machine.to_owned())
        } else {
            Err("token mismatch")
        }
    }

    /// Remaining block time, and whether this is the first rejection of the block.
    fn blocked_for(&self, ip: IpAddr) -> Option<(Duration, bool)> {
        let mut blocked = self.blocked.lock();
        let (until, logged) = blocked.get_mut(&ip)?;
        let now = Instant::now();
        if *until > now {
            let first = !std::mem::replace(logged, true);
            Some((*until - now, first))
        } else {
            blocked.remove(&ip);
            None
        }
    }

    fn record_failure(&self, ip: IpAddr) -> AuthRejection {
        if self.failures.len() > LIMITER_PRUNE_AT {
            self.failures.retain_recent();
        }
        match self.failures.check_key(&ip) {
            Ok(()) => AuthRejection::Unauthorized,
            Err(not_until) => {
                let retry_after = not_until.wait_time_from(self.clock.now());
                let now = Instant::now();
                let mut blocked = self.blocked.lock();
                if blocked.len() > LIMITER_PRUNE_AT {
                    blocked.retain(|_, (until, _)| *until > now);
                }
                blocked.insert(ip, (now + retry_after, false));
                warn!(%ip, retry_after_s = retry_after.as_secs(), "agent auth: rate limit engaged");
                AuthRejection::RateLimited { retry_after }
            }
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// `rd1.<machine>.<secret>` → `<machine>` (validated).
fn machine_of(token: &str) -> Option<&str> {
    let mut parts = token.splitn(3, '.');
    let (prefix, machine, secret) = (parts.next()?, parts.next()?, parts.next()?);
    (prefix == TOKEN_PREFIX && !secret.is_empty() && valid_machine_name(machine)).then_some(machine)
}

/// Authenticated viewer, inserted into request extensions by [`viewer_auth`].
#[derive(Debug, Clone)]
pub struct ViewerIdentity {
    pub user: String,
    /// `X-authentik-uid`, if Authentik sent one (never in dev mode).
    pub uid: Option<String>,
    pub ip: IpAddr,
}

pub const DEV_USER: &str = "dev@localhost";

/// Middleware for every route on the viewer port: `X-authentik-email` must be allowlisted and
/// WebSocket upgrades (and cross-site state-changing requests) must come from
/// `RD_PUBLIC_ORIGIN`. Dev mode skips the checks.
pub async fn viewer_auth(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = client_ip(req.headers(), peer);
    let (user, uid) = if st.cfg.dev {
        (DEV_USER.to_owned(), None)
    } else {
        match check_viewer(&st, &req) {
            Ok(id) => id,
            Err(reason) => {
                st.metrics.viewer_auth_failures.inc();
                let h = req.headers();
                warn!(
                    %ip,
                    path = req.uri().path(),
                    reason,
                    email = header_str(h, "x-authentik-email").unwrap_or("-"),
                    uid = header_str(h, "x-authentik-uid").unwrap_or("-"),
                    "viewer forbidden"
                );
                return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
            }
        }
    };
    req.extensions_mut()
        .insert(ViewerIdentity { user, uid, ip });
    next.run(req).await
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Returns the viewer's (email, uid).
fn check_viewer(st: &AppState, req: &Request) -> Result<(String, Option<String>), &'static str> {
    let email = header_str(req.headers(), "x-authentik-email")
        .map(str::to_ascii_lowercase)
        .ok_or("missing X-authentik-email")?;
    if !st.cfg.allowed_emails.contains(&email) {
        return Err("email not allowed");
    }
    let uid = header_str(req.headers(), "x-authentik-uid").map(str::to_owned);
    if !st.cfg.allowed_uids.is_empty()
        && !uid
            .as_ref()
            .is_some_and(|u| st.cfg.allowed_uids.contains(u))
    {
        return Err("uid not allowed");
    }
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let origin_ok = || {
        matches!((origin, st.cfg.public_origin.as_deref()),
            (Some(got), Some(want)) if got.trim_end_matches('/').eq_ignore_ascii_case(want))
    };
    let is_upgrade = req.uri().path().starts_with("/ws/")
        || req
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if is_upgrade && !origin_ok() {
        return Err("bad or missing Origin on WebSocket upgrade");
    }
    let safe_method = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe_method && origin.is_some() && !origin_ok() {
        return Err("cross-origin request");
    }
    Ok((email, uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(auth: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(auth).unwrap());
        h
    }

    #[test]
    fn token_parsing() {
        assert_eq!(machine_of("rd1.mbp.s3cret"), Some("mbp"));
        assert_eq!(machine_of("rd1.mbp.with.dots"), Some("mbp"));
        assert_eq!(machine_of("rd1.mbp."), None);
        assert_eq!(machine_of("rd2.mbp.x"), None);
        assert_eq!(machine_of("rd1.../x"), None);
        assert_eq!(machine_of("garbage"), None);
        assert_eq!(bearer_token(&headers("Bearer  abc ")), Some("abc"));
        assert_eq!(bearer_token(&headers("bearer abc")), Some("abc"));
        assert_eq!(bearer_token(&headers("Basic abc")), None);
        assert_eq!(bearer_token(&headers("Bearer ")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn verifies_hash_and_limits_failures() {
        let dir = std::env::temp_dir().join(format!("rd-gw-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token = "rd1.mbp.correct-horse";
        std::fs::write(
            dir.join("mbp"),
            format!("{}\n", hex::encode(Sha256::digest(token))),
        )
        .unwrap();
        let auth = AgentAuth::new(false, dir.clone());
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let ok = auth
            .verify(&headers(&format!("Bearer {token}")), ip, "/")
            .await;
        assert_eq!(
            ok,
            Ok(AgentIdentity {
                machine: Some("mbp".into())
            })
        );
        for _ in 0..AUTH_FAILURES_PER_MINUTE {
            let r = auth.verify(&headers("Bearer rd1.mbp.wrong"), ip, "/").await;
            assert_eq!(r, Err(AuthRejection::Unauthorized));
        }
        let r = auth.verify(&headers("Bearer rd1.mbp.wrong"), ip, "/").await;
        assert!(matches!(r, Err(AuthRejection::RateLimited { .. })));
        // Blocked IPs are refused even with a valid token; other IPs are unaffected.
        let r = auth
            .verify(&headers(&format!("Bearer {token}")), ip, "/")
            .await;
        assert!(matches!(r, Err(AuthRejection::RateLimited { .. })));
        let other: IpAddr = "192.0.2.2".parse().unwrap();
        assert!(
            auth.verify(&headers(&format!("Bearer {token}")), other, "/")
                .await
                .is_ok()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn dev_accepts_anything() {
        let auth = AgentAuth::new(true, PathBuf::from("/nonexistent"));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let id = auth
            .verify(&headers("Bearer rd1.mbp.x"), ip, "/")
            .await
            .unwrap();
        assert_eq!(id.machine.as_deref(), Some("mbp"));
        let id = auth.verify(&HeaderMap::new(), ip, "/").await.unwrap();
        assert_eq!(id.machine, None);
    }
}
