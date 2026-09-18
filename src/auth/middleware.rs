use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

use super::rate_limit::RateLimiter;
use super::tokens;
use crate::domain::ApiToken;
use crate::error::StoreError;
use crate::ports::tokens::TokenStore;
use crate::ports::users::UserStore;
use crate::registry::oci::token::{self, TokenSigner};

// ---------------------------------------------------------------------------
// Auth state (passed via axum's State extractor)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuthState {
    pub static_tokens: Vec<String>,
    pub anonymous_read: bool,
    pub users: Arc<dyn UserStore>,
    pub tokens: Arc<dyn TokenStore>,
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
    /// The API token's display name when one identified the caller.
    pub token_name: Option<String>,
}

impl AuthUser {
    fn from_user(user: crate::domain::User, token: &str, token_name: Option<String>) -> Self {
        Self {
            token: token.to_string(),
            user_id: Some(user.id),
            username: user.username,
            role: user.role,
            must_change_password: user.must_change_password,
            token_name,
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
                tracing::warn!(error = %e, "store error during bearer authentication");
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
    let user = state.users.by_name(username).await.map_err(|e| {
        tracing::warn!(error = %e, "store error during basic authentication");
        AuthFailure::Unavailable
    })?;
    if let Some(user) = user {
        let password_ok =
            super::users::verify_password_async(password.to_string(), user.password_hash.clone())
                .await
                .unwrap_or(false);
        if password_ok {
            return Ok(Some(AuthUser::from_user(user, "", None)));
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
    let mut token_name = None;
    if let Some(id) = claims.api_token_id.as_deref() {
        match live_api_token_by_id(state, id).await {
            Ok(Some(api_token)) => token_name = Some(api_token.name),
            Ok(None) => return unauthorized_response(is_oci),
            Err(e) => {
                tracing::warn!(error = %e, "store error during registry token authentication");
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
    match state.users.by_name(&username).await {
        Ok(Some(user)) => run_as(AuthUser::from_user(user, raw, token_name), request, next).await,
        Ok(None) => unauthorized_response(is_oci),
        Err(e) => {
            tracing::warn!(error = %e, "store error during registry token authentication");
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
/// expired); `Err` means the store could not answer and the caller must NOT
/// treat the token as invalid.
pub(crate) async fn authenticate_bearer(
    state: &AuthState,
    token: &str,
) -> Result<Option<AuthUser>, StoreError> {
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
    try_stored_token_auth(state, token).await
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
        token_name: None,
    }
}

/// Attempt to authenticate via a stored API token.
///
/// Looks up the token by its prefix, verifies the hash, checks expiration,
/// loads the user, and records the use. Store errors are propagated instead
/// of being collapsed into a rejection (the historical "401 under load":
/// SQLITE_BUSY looked like an invalid token).
async fn try_stored_token_auth(
    state: &AuthState,
    raw_token: &str,
) -> Result<Option<AuthUser>, StoreError> {
    let Some(stored) = live_api_token(state, raw_token).await? else {
        return Ok(None);
    };
    let Some(user) = state.users.by_id(stored.user_id).await? else {
        return Ok(None);
    };
    let _ = state.tokens.touch(&stored.id, Utc::now()).await;
    Ok(Some(AuthUser::from_user(
        user,
        raw_token,
        Some(stored.name),
    )))
}

/// The API token behind `raw_token`, if it exists, matches and is still live.
pub(crate) async fn live_api_token(
    state: &AuthState,
    raw_token: &str,
) -> Result<Option<ApiToken>, StoreError> {
    if raw_token.len() < 16 {
        return Ok(None);
    }
    let Some(stored) = state.tokens.by_prefix(&raw_token[..16]).await? else {
        return Ok(None);
    };
    if !tokens::verify_token(raw_token, &stored.token_hash) || !stored.is_live(Utc::now()) {
        return Ok(None);
    }
    Ok(Some(stored))
}

/// The API token behind `id`, `None` when missing or expired: one lookup, so
/// a registry-token request learns the name for free.
pub(crate) async fn live_api_token_by_id(
    state: &AuthState,
    id: &str,
) -> Result<Option<ApiToken>, StoreError> {
    let stored = state.tokens.by_id(id).await?;
    Ok(stored.filter(|token| token.is_live(Utc::now())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::tokens::NewToken;
    use crate::ports::users::NewUser;
    use crate::testing::fakes::FakeDb;
    use chrono::Duration;

    fn state(db: &FakeDb) -> AuthState {
        AuthState {
            static_tokens: Vec::new(),
            anonymous_read: false,
            users: db.users(),
            tokens: db.tokens(),
            login_rate_limiter: Arc::new(RateLimiter::new(5, 60)),
            base_url: "http://localhost".to_string(),
            registry_tokens: TokenSigner::random(),
        }
    }

    /// Issues a token the way `POST /users/{u}/tokens` does, `expires_in_days`
    /// included, and hands back its raw value.
    async fn issue(db: &FakeDb, id: &str, expires_in_days: Option<i64>) -> String {
        let now = Utc::now();
        let user = db
            .users()
            .by_name("ci")
            .await
            .unwrap()
            .expect("the fixture user exists");
        let (raw, hash) = tokens::generate_token("trg_");
        db.tokens()
            .create(
                &NewToken {
                    id,
                    user_id: user.id,
                    name: "ci-runner",
                    prefix: &raw[..16],
                    token_hash: &hash,
                    expires_at: expires_in_days.map(|days| now + Duration::days(days)),
                },
                now,
            )
            .await
            .unwrap();
        raw
    }

    async fn with_user() -> FakeDb {
        let db = FakeDb::new();
        db.users()
            .create(
                &NewUser {
                    username: "ci",
                    email: None,
                    password_hash: "hash",
                    role: "reader",
                },
                Utc::now(),
            )
            .await
            .unwrap();
        db
    }

    /// The expiry a caller asked for in days is enforced as an instant, not
    /// as a string in one adapter's spelling.
    #[tokio::test]
    async fn a_token_is_accepted_before_its_expiry_and_rejected_after() {
        let db = with_user().await;
        let state = state(&db);

        let live = issue(&db, "t-live", Some(30)).await;
        let dead = issue(&db, "t-dead", Some(-1)).await;
        let forever = issue(&db, "t-forever", None).await;

        for raw in [&live, &forever] {
            let user = authenticate_bearer(&state, raw)
                .await
                .unwrap()
                .expect("a live token authenticates");
            assert_eq!(
                (user.username.as_str(), user.token_name.as_deref()),
                ("ci", Some("ci-runner"))
            );
        }
        assert!(
            authenticate_bearer(&state, &dead).await.unwrap().is_none(),
            "an expired token is rejected, not accepted"
        );
    }

    #[tokio::test]
    async fn authenticating_records_the_use_but_a_wrong_hash_does_not() {
        let db = with_user().await;
        let state = state(&db);
        let raw = issue(&db, "t-live", None).await;

        assert!(authenticate_bearer(&state, &raw).await.unwrap().is_some());
        let stored = db.tokens().by_id("t-live").await.unwrap().unwrap();
        assert!(stored.last_used_at.is_some(), "the use is recorded");

        let forged = format!("{}ffff", &raw[..raw.len() - 4]);
        assert!(authenticate_bearer(&state, &forged).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn live_api_token_by_id_returns_name_and_rejects_expired() {
        let db = with_user().await;
        let state = state(&db);
        issue(&db, "t-live", None).await;
        issue(&db, "t-dead", Some(-1)).await;

        let live = live_api_token_by_id(&state, "t-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.name, "ci-runner");
        assert!(live_api_token_by_id(&state, "t-dead")
            .await
            .unwrap()
            .is_none());
        assert!(live_api_token_by_id(&state, "nope").await.unwrap().is_none());
    }
}
