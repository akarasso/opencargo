use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

use super::rate_limit::RateLimiter;
use super::tokens;

// ---------------------------------------------------------------------------
// Auth state (passed via axum's State extractor)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuthState {
    pub static_tokens: Vec<String>,
    pub anonymous_read: bool,
    pub db: SqlitePool,
    /// Shared with `AppState.login_rate_limiter`; throttles Basic Auth attempts.
    pub login_rate_limiter: Arc<RateLimiter>,
}

// ---------------------------------------------------------------------------
// Authenticated user, inserted into request extensions on success
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub token: String,
    pub user_id: Option<i64>,
    pub username: String,
    pub role: String,
    /// When true, the user must change their password before doing anything
    /// other than changing it (enforced in `auth_middleware`).
    pub must_change_password: bool,
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Axum middleware for Bearer token authentication.
///
/// Supports both static tokens from config (backwards compat) and
/// DB-backed API tokens with user lookup.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let is_read = request.method() == axum::http::Method::GET
        || request.method() == axum::http::Method::HEAD;

    // Detect if this is an OCI request (Docker client) for proper Www-Authenticate headers
    let is_oci = request.uri().path().starts_with("/v2");

    // Try to extract the Bearer token from the Authorization header.
    let bearer_token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|t| t.to_string());

    // Try Basic Auth if no Bearer token
    let basic_auth = if bearer_token.is_none() {
        request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b64| {
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            })
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|decoded| {
                let parts: Vec<&str> = decoded.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
    } else {
        None
    };

    // Handle Basic Auth
    if let Some((username, password)) = basic_auth {
        // Throttle Basic Auth to stop brute force and the Argon2 CPU DoS it
        // enables — but only count FAILED attempts (recorded below), so a
        // legitimate client making many authenticated requests in a burst
        // (e.g. `docker push`) is never throttled. Checked before Argon2 so a
        // username already over its failure budget can't keep burning CPU.
        let rl_key = format!("basic:{username}");
        if state.login_rate_limiter.is_limited(&rl_key) {
            return too_many_requests_response();
        }
        if let Ok(Some(user)) = crate::db::get_user_by_username(&state.db, &username).await {
            let password_ok =
                super::users::verify_password_async(password, user.password_hash.clone())
                    .await
                    .unwrap_or(false);
            if password_ok {
                let auth_user = AuthUser {
                    token: String::new(),
                    user_id: Some(user.id),
                    username: user.username,
                    role: user.role,
                    must_change_password: user.must_change_password == 1,
                };
                if let Some(resp) = password_change_pending_block(
                    &auth_user,
                    request.method(),
                    request.uri().path(),
                ) {
                    return resp;
                }
                let mut request = request;
                request.extensions_mut().insert(auth_user);
                return next.run(request).await;
            }
        }
        // Basic Auth failed (unknown user or bad password): record the failed
        // attempt so repeated failures from this username get throttled.
        state.login_rate_limiter.record_failure(&rl_key);
        return unauthorized_response(is_oci);
    }

    match bearer_token {
        Some(t) => {
            // 1. Check static tokens first (backwards compatibility)
            // Use constant-time comparison to prevent timing attacks
            let is_static = state.static_tokens.iter().any(|st| {
                st.len() == t.len()
                    && st
                        .bytes()
                        .zip(t.bytes())
                        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                        == 0
            });
            if is_static {
                let mut request = request;
                request.extensions_mut().insert(AuthUser {
                    token: t,
                    user_id: None,
                    username: "static-token".to_string(),
                    role: "admin".to_string(),
                    must_change_password: false,
                });
                return next.run(request).await;
            }

            // 2. Positive auth cache: a burst of requests carrying the same token
            //    (e.g. `pnpm install` pulling ~1000 tarballs, or several parallel
            //    image builds at once) then does ONE DB lookup instead of one per
            //    request — keeping the SQLite auth path off the hot path under
            //    concurrency, which is what produced the intermittent 401s.
            if let Some(auth_user) = auth_cache_get(&t) {
                if let Some(resp) = password_change_pending_block(
                    &auth_user,
                    request.method(),
                    request.uri().path(),
                ) {
                    return resp;
                }
                let mut request = request;
                request.extensions_mut().insert(auth_user);
                return next.run(request).await;
            }

            // 3. DB token lookup (prefix -> verify hash -> load user), then cache it.
            match try_db_token_auth(&state.db, &t).await {
                Ok(Some(auth_user)) => {
                    auth_cache_put(&t, &auth_user);
                    if let Some(resp) = password_change_pending_block(
                        &auth_user,
                        request.method(),
                        request.uri().path(),
                    ) {
                        return resp;
                    }
                    let mut request = request;
                    request.extensions_mut().insert(auth_user);
                    return next.run(request).await;
                }
                // Genuine "no such token" — fall through to anonymous/401 below.
                Ok(None) => {}
                // Transient DB error (e.g. SQLITE_BUSY under load): a valid token
                // must NOT be reported as invalid. Return 503 so the client
                // retries instead of aborting on a fatal 401.
                Err(e) => {
                    tracing::warn!("token auth DB error (returning 503): {e}");
                    return service_unavailable_response();
                }
            }

            // Token not valid
            if state.anonymous_read && is_read {
                next.run(request).await
            } else {
                unauthorized_response(is_oci)
            }
        }
        None => {
            // No token. Allow anonymous GET if configured.
            if state.anonymous_read && is_read {
                next.run(request).await
            } else {
                unauthorized_response(is_oci)
            }
        }
    }
}

/// Build an unauthorized response. For OCI/Docker requests, include the
/// `Www-Authenticate` header so Docker knows to send credentials.
fn unauthorized_response(is_oci: bool) -> Response {
    if is_oci {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"opencargo\"")],
            Json(json!({
                "errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "Invalid or missing authentication token"
            })),
        )
            .into_response()
    }
}

/// 429 response for throttled authentication attempts.
fn too_many_requests_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": "too many authentication attempts, try again later"})),
    )
        .into_response()
}

/// 503 response for a transient auth-path DB error (e.g. SQLITE_BUSY under load).
/// A retryable status — never surface a lock contention as a 401, which npm/pnmp
/// treat as a fatal auth failure and abort the whole install.
fn service_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, "1")],
        Json(json!({"error": "temporary error, please retry"})),
    )
        .into_response()
}

/// While `must_change_password` is set, allow only the password-change endpoint
/// (`PUT .../password`) and `/-/whoami`; block everything else with 403. This
/// makes the forced rotation real instead of cosmetic — a long-lived token can
/// no longer be used indefinitely without changing the password.
fn password_change_pending_block(
    auth_user: &AuthUser,
    method: &axum::http::Method,
    path: &str,
) -> Option<Response> {
    if !auth_user.must_change_password {
        return None;
    }
    let allowed =
        (method == axum::http::Method::PUT && path.ends_with("/password")) || path == "/-/whoami";
    if allowed {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "password change required before using the API"})),
            )
                .into_response(),
        )
    }
}

/// Attempt to authenticate via a DB API token.
///
/// Looks up the token by its prefix, verifies the hash, checks expiration,
/// loads the user, and updates `last_used_at`.
async fn try_db_token_auth(
    db: &SqlitePool,
    raw_token: &str,
) -> Result<Option<AuthUser>, sqlx::Error> {
    // The prefix stored in DB is the first 16 characters of the raw token.
    if raw_token.len() < 16 {
        return Ok(None);
    }
    let prefix = &raw_token[..16];

    // Under a heavy concurrent read burst (several parallel `pnpm install`) the
    // SQLite read can transiently yield an EMPTY result — or a busy error — for a
    // token row that provably exists, which used to surface as a spurious 401 that
    // aborts the whole install. Retry a few times with a short backoff. A
    // genuinely-absent token returns None on every attempt (then a real 401); a
    // persistent DB error still bubbles up as Err -> 503 (retryable).
    let db_token = {
        let mut found = None;
        for attempt in 0..4u32 {
            match crate::db::get_token_by_prefix(db, prefix).await {
                Ok(Some(t)) => {
                    found = Some(t);
                    break;
                }
                Ok(None) => {} // possibly transient under load — retry below
                Err(e) if attempt == 3 => return Err(e),
                Err(e) => tracing::warn!("token prefix lookup error (attempt {attempt}): {e}"),
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        match found {
            Some(t) => t,
            None => {
                tracing::warn!("AUTH-DIAG: token prefix {prefix} unresolved after 4 attempts");
                return Ok(None);
            }
        }
    };

    // Verify the token hash
    if !tokens::verify_token(raw_token, &db_token.token_hash) {
        return Ok(None);
    }

    // Check expiration. Fail CLOSED: an unparseable timestamp rejects the token
    // rather than silently treating it as non-expiring (the previous behaviour).
    if let Some(ref expires_at) = db_token.expires_at {
        match chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S") {
            Ok(exp) if exp < chrono::Utc::now().naive_utc() => return Ok(None), // expired
            Ok(_) => {}                                                         // still valid
            Err(_) => return Ok(None), // corrupt/unexpected format -> reject
        }
    }

    // Load the user (same transient-empty-read resilience as the token lookup).
    let user = {
        let mut found = None;
        for attempt in 0..4u32 {
            match sqlx::query_as::<_, crate::db::User>("SELECT * FROM users WHERE id = ?1")
                .bind(db_token.user_id)
                .fetch_optional(db)
                .await
            {
                Ok(Some(u)) => {
                    found = Some(u);
                    break;
                }
                Ok(None) => {}
                Err(e) if attempt == 3 => return Err(e),
                Err(e) => tracing::warn!("user lookup error (attempt {attempt}): {e}"),
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        match found {
            Some(u) => u,
            None => {
                tracing::warn!("AUTH-DIAG: user {} unresolved after 4 attempts", db_token.user_id);
                return Ok(None);
            }
        }
    };

    // Update last_used_at, throttled to at most once per minute per token. The
    // previous unconditional write ran on EVERY authenticated request, turning a
    // tarball-download burst into a write storm that starved concurrent auth
    // reads. Best-effort — a failed update never blocks the request.
    if should_write_last_used(&db_token.id) {
        let _ = crate::db::update_token_last_used(db, &db_token.id).await;
    }

    Ok(Some(AuthUser {
        token: raw_token.to_string(),
        user_id: Some(user.id),
        username: user.username,
        role: user.role,
        must_change_password: user.must_change_password == 1,
    }))
}

/// Per-token throttle for `last_used_at` writes: token id -> last write instant.
static LAST_USED_WRITES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Returns true at most once per minute per token id (recording the write time),
/// so the per-request `last_used_at` update can't become a write storm under a
/// concurrent download burst.
fn should_write_last_used(token_id: &str) -> bool {
    use std::time::{Duration, Instant};
    let mut map = LAST_USED_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    match map.get(token_id) {
        Some(&last) if now.duration_since(last) < Duration::from_secs(60) => false,
        _ => {
            map.insert(token_id.to_string(), now);
            true
        }
    }
}

/// Positive auth cache: raw token -> (resolved user, cached-at). A verified token
/// is trusted for up to `AUTH_CACHE_TTL` so a burst of requests carrying it does a
/// single DB lookup rather than one per request. Trade-off: a revoked/expired
/// token stays honoured for at most the TTL — acceptable for long-lived registry
/// tokens, and the fix for the concurrency-induced 401 storms.
static AUTH_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (AuthUser, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const AUTH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Returns a cached `AuthUser` for `raw_token` if verified within the TTL.
fn auth_cache_get(raw_token: &str) -> Option<AuthUser> {
    let mut cache = AUTH_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match cache.get(raw_token) {
        Some((user, at)) if at.elapsed() < AUTH_CACHE_TTL => Some(user.clone()),
        Some(_) => {
            cache.remove(raw_token);
            None
        }
        None => None,
    }
}

/// Records a freshly-verified token so subsequent requests skip the DB.
fn auth_cache_put(raw_token: &str, user: &AuthUser) {
    let mut cache = AUTH_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.insert(raw_token.to_string(), (user.clone(), std::time::Instant::now()));
}

/// Drops every cached auth entry. Called whenever a token is revoked so the
/// positive cache can never keep a revoked credential alive — revocation stays
/// effective immediately, not after the TTL. The cache is keyed by raw token
/// (which revocation, working by id, does not have), so a full clear is the
/// correct coarse invalidation; revocations are rare.
pub fn clear_auth_cache() {
    AUTH_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}
