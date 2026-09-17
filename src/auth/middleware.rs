use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
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
use crate::registry::oci::token::{self, TokenSigner};

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
    /// Names the token realm in every OCI challenge.
    pub base_url: String,
    pub registry_tokens: TokenSigner,
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

impl AuthUser {
    fn from_user(user: crate::db::User, token: &str) -> Self {
        Self {
            token: token.to_string(),
            user_id: Some(user.id),
            username: user.username,
            role: user.role,
            must_change_password: user.must_change_password == 1,
        }
    }
}

/// Why credentials could not be checked, as opposed to being wrong.
pub(crate) enum AuthFailure {
    Throttled,
    Unavailable,
}

impl IntoResponse for AuthFailure {
    fn into_response(self) -> Response {
        match self {
            AuthFailure::Throttled => too_many_requests_response(),
            AuthFailure::Unavailable => service_unavailable_response(),
        }
    }
}

enum Credentials {
    Basic(String, String),
    Registry(String),
    Bearer(String),
    None,
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Axum middleware for Bearer and Basic authentication.
///
/// Bearer values are static config tokens, DB-backed API tokens or registry
/// tokens issued by `/v2/token`; Basic credentials are checked against the
/// user's password. Every 401 under `/v2/` carries the Bearer challenge that
/// points Docker clients at the token endpoint.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let challenge = request
        .uri()
        .path()
        .starts_with("/v2/")
        .then(|| token::challenge(&state.base_url, request.method(), request.uri().path()))
        .flatten();
    let mut response = authenticate(&state, request, next).await;
    if let Some(challenge) = challenge.filter(|_| response.status() == StatusCode::UNAUTHORIZED) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

async fn authenticate(state: &AuthState, request: Request<Body>, next: Next) -> Response {
    let is_oci = is_oci_path(request.uri().path());
    match credentials(request.headers()) {
        Credentials::Basic(username, password) => {
            match authenticate_basic(state, &username, &password).await {
                Ok(Some(auth_user)) => run_as(auth_user, request, next).await,
                Ok(None) => unauthorized_response(is_oci),
                Err(failure) => failure.into_response(),
            }
        }
        Credentials::Registry(_) if !request.uri().path().starts_with("/v2/") => {
            unauthorized_response(false)
        }
        Credentials::Registry(raw) => run_registry_token(state, &raw, is_oci, request, next).await,
        Credentials::Bearer(raw) => match authenticate_bearer(state, &raw).await {
            Ok(Some(auth_user)) => run_as(auth_user, request, next).await,
            Ok(None) => run_anonymous(state, request, next).await,
            Err(e) => {
                tracing::warn!(error = %e, "database error during bearer authentication");
                service_unavailable_response()
            }
        },
        Credentials::None if is_read(&request) && is_cargo_config_path(request.uri().path()) => {
            next.run(request).await
        }
        Credentials::None => run_anonymous(state, request, next).await,
    }
}

fn credentials(headers: &HeaderMap) -> Credentials {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Credentials::None;
    };
    if let Some(raw) = value.strip_prefix("Bearer ") {
        if raw.starts_with(token::PREFIX) {
            return Credentials::Registry(raw.to_string());
        }
        return Credentials::Bearer(raw.to_string());
    }
    match basic_credentials(value) {
        Some((username, password)) => Credentials::Basic(username, password),
        None => Credentials::None,
    }
}

/// The `(username, password)` of a `Basic` header value, if it decodes.
pub(crate) fn basic_credentials(value: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.strip_prefix("Basic ")?)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Check a password, counting only failures against the login rate limiter
/// so a burst of authenticated requests (a `docker push`) is never throttled.
/// A DB failure says nothing about the credentials: it is reported as
/// unavailable and not counted.
pub(crate) async fn authenticate_basic(
    state: &AuthState,
    username: &str,
    password: &str,
) -> Result<Option<AuthUser>, AuthFailure> {
    let rl_key = format!("basic:{username}");
    if state.login_rate_limiter.is_limited(&rl_key) {
        return Err(AuthFailure::Throttled);
    }
    let user = crate::db::get_user_by_username(&state.db, username)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "database error during basic authentication");
            AuthFailure::Unavailable
        })?;
    if let Some(user) = user {
        let password_ok =
            super::users::verify_password_async(password.to_string(), user.password_hash.clone())
                .await
                .unwrap_or(false);
        if password_ok {
            return Ok(Some(AuthUser::from_user(user, "")));
        }
    }
    state.login_rate_limiter.record_failure(&rl_key);
    Ok(None)
}

/// A registry token names its user, or nobody: an anonymous token walks the
/// anonymous path, one issued to a static config token is the same synthetic
/// admin, and a named one is loaded like a DB token.
async fn run_registry_token(
    state: &AuthState,
    raw: &str,
    is_oci: bool,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Some(claims) = state.registry_tokens.verify(raw) else {
        return unauthorized_response(is_oci);
    };
    if let Some(id) = claims.api_token_id.as_deref() {
        match api_token_is_live(&state.db, id).await {
            Ok(true) => {}
            Ok(false) => return unauthorized_response(is_oci),
            Err(e) => {
                tracing::warn!(error = %e, "database error during registry token authentication");
                return service_unavailable_response();
            }
        }
    }
    let subject = claims.sub.clone();
    let static_token = claims.static_token;
    request.extensions_mut().insert(claims);
    if static_token {
        return run_as(static_token_user(raw), request, next).await;
    }
    let Some(username) = subject else {
        return run_anonymous(state, request, next).await;
    };
    match crate::db::get_user_by_username(&state.db, &username).await {
        Ok(Some(user)) => run_as(AuthUser::from_user(user, raw), request, next).await,
        Ok(None) => unauthorized_response(is_oci),
        Err(e) => {
            tracing::warn!(error = %e, "database error during registry token authentication");
            service_unavailable_response()
        }
    }
}

async fn run_as(auth_user: AuthUser, mut request: Request<Body>, next: Next) -> Response {
    if let Some(resp) =
        password_change_pending_block(&auth_user, request.method(), request.uri().path())
    {
        return resp;
    }
    request.extensions_mut().insert(auth_user);
    next.run(request).await
}

async fn run_anonymous(state: &AuthState, request: Request<Body>, next: Next) -> Response {
    if state.anonymous_read && is_read(&request) {
        next.run(request).await
    } else {
        unauthorized_response(is_oci_path(request.uri().path()))
    }
}

fn is_read(request: &Request<Body>) -> bool {
    request.method() == axum::http::Method::GET || request.method() == axum::http::Method::HEAD
}

fn is_oci_path(path: &str) -> bool {
    path.starts_with("/v2/")
}

/// `/{repo}/index/config.json`: cargo reads it before it knows whether to send
/// a token and learns to from `auth-required`, so a tokenless read passes the
/// anonymous gate; the handler discloses only existence and format.
fn is_cargo_config_path(path: &str) -> bool {
    let mut segments = path.split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some(""), Some(repo), Some("index"), Some("config.json"), None) if !repo.is_empty()
    )
}

/// Build an unauthorized response; OCI requests get the distribution error
/// body, and `auth_middleware` adds the Bearer challenge on top.
fn unauthorized_response(is_oci: bool) -> Response {
    if is_oci {
        (
            StatusCode::UNAUTHORIZED,
            [("Docker-Distribution-Api-Version", "registry/2.0")],
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

/// 503 response for transient DB failures during authentication — retryable,
/// unlike the 401 these used to be silently collapsed into.
fn service_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "authentication temporarily unavailable, try again"})),
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
///
/// `Ok(None)` means the token was actually rejected (unknown, bad hash,
/// expired); `Err` means the DB could not answer and the caller must NOT
/// treat the token as invalid.
pub(crate) async fn authenticate_bearer(
    state: &AuthState,
    token: &str,
) -> Result<Option<AuthUser>, sqlx::Error> {
    let is_static = state.static_tokens.iter().any(|st| {
        st.len() == token.len()
            && st
                .bytes()
                .zip(token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if is_static {
        return Ok(Some(static_token_user(token)));
    }
    try_db_token_auth(&state.db, token).await
}

/// The synthetic admin a static config token acts as; it has no row, so
/// `user_id` is `None` and `username` is a fixed label.
fn static_token_user(token: &str) -> AuthUser {
    AuthUser {
        token: token.to_string(),
        user_id: None,
        username: "static-token".to_string(),
        role: "admin".to_string(),
        must_change_password: false,
    }
}

/// Attempt to authenticate via a DB API token.
///
/// Looks up the token by its prefix, verifies the hash, checks expiration,
/// loads the user, and updates `last_used_at`. DB errors are propagated
/// instead of being collapsed into a rejection (the historical "401 under
/// load": SQLITE_BUSY looked like an invalid token).
async fn try_db_token_auth(
    db: &SqlitePool,
    raw_token: &str,
) -> Result<Option<AuthUser>, sqlx::Error> {
    let Some(db_token) = live_api_token(db, raw_token).await? else {
        return Ok(None);
    };
    let Some(user) =
        sqlx::query_as::<_, crate::db::User>("SELECT * FROM users WHERE id = ?1")
            .bind(db_token.user_id)
            .fetch_optional(db)
            .await?
    else {
        return Ok(None);
    };
    let _ = crate::db::update_token_last_used(db, &db_token.id).await;
    Ok(Some(AuthUser::from_user(user, raw_token)))
}

/// The API token row behind `raw_token`, if it exists, matches and has not expired.
pub(crate) async fn live_api_token(
    db: &SqlitePool,
    raw_token: &str,
) -> Result<Option<crate::db::ApiToken>, sqlx::Error> {
    if raw_token.len() < 16 {
        return Ok(None);
    }
    let Some(db_token) = crate::db::get_token_by_prefix(db, &raw_token[..16]).await? else {
        return Ok(None);
    };
    if !tokens::verify_token(raw_token, &db_token.token_hash) || expired(db_token.expires_at.as_deref()) {
        return Ok(None);
    }
    Ok(Some(db_token))
}

/// Whether the API token `id` still exists and has not expired.
pub(crate) async fn api_token_is_live(db: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT expires_at FROM api_tokens WHERE id = ?1")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(matches!(row, Some((expires_at,)) if !expired(expires_at.as_deref())))
}

// Fail closed: an unparseable timestamp counts as expired.
fn expired(expires_at: Option<&str>) -> bool {
    match expires_at {
        None => false,
        Some(text) => chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
            .map(|exp| exp < chrono::Utc::now().naive_utc())
            .unwrap_or(true),
    }
}
