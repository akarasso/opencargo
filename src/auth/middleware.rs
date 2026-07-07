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
            // Static config tokens (constant-time compare), then DB-backed
            // API tokens — shared with the WebSocket first-frame auth.
            if let Some(auth_user) = authenticate_bearer(&state, &t).await {
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

/// Resolve a Bearer token to an [`AuthUser`].
///
/// Static config tokens are checked first with a constant-time comparison
/// (they map to a synthetic admin user, backwards compat), then DB-backed
/// API tokens. Shared by the HTTP auth middleware and the WebSocket
/// first-frame authentication (`api::ws`).
pub(crate) async fn authenticate_bearer(state: &AuthState, token: &str) -> Option<AuthUser> {
    let is_static = state.static_tokens.iter().any(|st| {
        st.len() == token.len()
            && st
                .bytes()
                .zip(token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if is_static {
        return Some(AuthUser {
            token: token.to_string(),
            user_id: None,
            username: "static-token".to_string(),
            role: "admin".to_string(),
            must_change_password: false,
        });
    }
    try_db_token_auth(&state.db, token).await
}

/// Attempt to authenticate via a DB API token.
///
/// Looks up the token by its prefix, verifies the hash, checks expiration,
/// loads the user, and updates `last_used_at`.
async fn try_db_token_auth(db: &SqlitePool, raw_token: &str) -> Option<AuthUser> {
    // The prefix stored in DB is the first 16 characters of the raw token.
    if raw_token.len() < 16 {
        return None;
    }
    let prefix = &raw_token[..16];

    let db_token = crate::db::get_token_by_prefix(db, prefix).await.ok()??;

    // Verify the token hash
    if !tokens::verify_token(raw_token, &db_token.token_hash) {
        return None;
    }

    // Check expiration. Fail CLOSED: an unparseable timestamp rejects the token
    // rather than silently treating it as non-expiring (the previous behaviour).
    if let Some(ref expires_at) = db_token.expires_at {
        match chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S") {
            Ok(exp) if exp < chrono::Utc::now().naive_utc() => return None, // expired
            Ok(_) => {}                                                     // still valid
            Err(_) => return None, // corrupt/unexpected format -> reject
        }
    }

    // Load the user
    let user = sqlx::query_as::<_, crate::db::User>("SELECT * FROM users WHERE id = ?1")
        .bind(db_token.user_id)
        .fetch_optional(db)
        .await
        .ok()??;

    // Update last_used_at (fire-and-forget)
    let _ = crate::db::update_token_last_used(db, &db_token.id).await;

    Some(AuthUser {
        token: raw_token.to_string(),
        user_id: Some(user.id),
        username: user.username,
        role: user.role,
        must_change_password: user.must_change_password == 1,
    })
}
