//! SSO over HTTP: the browser's redirects, the SPA's JSON calls, and the
//! admin's controls over identities. The attempt cookie is `__Host-` with
//! `Secure`, `Path=/`, no `Domain`, `HttpOnly` and `SameSite=Lax`; only the
//! loopback development flag drops the prefix and `Secure`.

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::{record_audit, require_admin, require_admin_or_self, require_auth};
use crate::app::authenticate::Refusal;
use crate::app::sso::{Meta, SsoError};
use crate::auth::middleware::source_of;
use crate::domain::identity::{Authority, IdentityKey, PasswordMode, SsoRefusal};
use crate::error::{AppError, AppResult};
use crate::ports::identity_provider::Callback;
use crate::server::AppState;

const COOKIE: &str = "__Host-oc_sso";
const DEV_COOKIE: &str = "oc_sso";
const COOKIE_MAX_AGE: i64 = 600;

/// The routes a browser reaches before it holds any credential.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/sso/providers", get(providers))
        .route("/api/v1/auth/sso/exchange", post(exchange))
        .route("/api/v1/auth/sso/{provider}/start", get(start))
        .route("/api/v1/auth/sso/{provider}/callback", get(callback))
}

/// The routes behind the auth middleware.
pub fn user_routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/sso/link/start", post(link_start))
        .route("/api/v1/auth/sso/link/pending", get(link_pending))
        .route("/api/v1/auth/sso/link/confirm", post(link_confirm))
        .route("/api/v1/auth/logout", post(logout))
        .route(
            "/api/v1/users/{username}/identities",
            get(list_identities).delete(unlink),
        )
        .route(
            "/api/v1/users/{username}/identities/disable",
            post(disable_identity),
        )
        .route("/api/v1/users/{username}/disable", post(disable_user))
        .route("/api/v1/users/{username}/enable", post(enable_user))
}

fn cookie_name(state: &AppState) -> &'static str {
    if state.sso_insecure_cookies {
        DEV_COOKIE
    } else {
        COOKIE
    }
}

fn set_cookie(state: &AppState, value: &str, max_age: i64) -> HeaderValue {
    let secure = if state.sso_insecure_cookies {
        ""
    } else {
        "; Secure"
    };
    HeaderValue::from_str(&format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        cookie_name(state)
    ))
    .expect("a sealed cookie is header-safe")
}

fn read_cookie(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let name = cookie_name(state);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

struct Client {
    ip: String,
    user_agent: Option<String>,
}

impl Client {
    fn of(state: &AppState, request: &Request<Body>) -> Self {
        let headers = request.headers();
        let peer = request
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0);
        Self {
            ip: source_of(peer.map(|p| p.ip()), headers, &state.auth.trusted_proxies),
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        }
    }

    fn meta(&self) -> Meta<'_> {
        Meta {
            ip: Some(&self.ip),
            user_agent: self.user_agent.as_deref(),
        }
    }
}

fn status_of(refusal: SsoRefusal) -> StatusCode {
    match refusal {
        SsoRefusal::UnknownProvider => StatusCode::NOT_FOUND,
        SsoRefusal::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        SsoRefusal::IdpError => StatusCode::BAD_GATEWAY,
        SsoRefusal::NameCollision | SsoRefusal::AlreadyLinked => StatusCode::CONFLICT,
        SsoRefusal::Denied
        | SsoRefusal::Disabled
        | SsoRefusal::Rejected
        | SsoRefusal::AdminNotLinkable
        | SsoRefusal::WrongUser => StatusCode::FORBIDDEN,
        SsoRefusal::StateMismatch
        | SsoRefusal::HandoffUnknown
        | SsoRefusal::HandoffConsumed
        | SsoRefusal::HandoffExpired
        | SsoRefusal::BindingMismatch => StatusCode::UNAUTHORIZED,
    }
}

fn json_error(err: SsoError) -> Response {
    let (status, code) = match err {
        SsoError::Refused(r) => (status_of(r), r.as_str()),
        SsoError::Credentials(Refusal::Invalid) => (StatusCode::UNAUTHORIZED, "bad_credentials"),
        SsoError::Credentials(Refusal::Throttled) => (StatusCode::TOO_MANY_REQUESTS, "throttled"),
        SsoError::Credentials(Refusal::Unavailable) | SsoError::Store(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    };
    (status, Json(json!({ "error": code }))).into_response()
}

/// A browser landing: refusals go back to the login screen, named.
fn browser_error(err: SsoError) -> Response {
    match err {
        SsoError::Store(_) | SsoError::Credentials(Refusal::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "SSO temporarily unavailable",
        )
            .into_response(),
        SsoError::Refused(r) => see_other(&format!("/login?sso_error={}", r.as_str()), None),
        SsoError::Credentials(_) => see_other("/login?sso_error=bad_credentials", None),
    }
}

fn see_other(location: &str, cookie: Option<HeaderValue>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, location);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(cookie) = cookie {
        headers.insert(header::SET_COOKIE, cookie);
    }
    response
}

/// A body only a script can send: a cross-site form cannot set this
/// content type without a preflight.
fn not_json(headers: &HeaderMap) -> Option<Response> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().map(str::trim) == Some("application/json"));
    if is_json {
        None
    } else {
        Some((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error": "application/json required"})),
        )
            .into_response())
    }
}

async fn providers(State(state): State<AppState>) -> impl IntoResponse {
    let mode = match state.password_mode {
        PasswordMode::Enabled => "enabled",
        PasswordMode::AdminsOnly => "admins_only",
        PasswordMode::Disabled => "disabled",
    };
    Json(json!({ "providers": state.sso.provider_names(), "password_mode": mode }))
}

#[derive(Deserialize)]
struct StartQuery {
    return_to: Option<String>,
}

async fn start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<StartQuery>,
) -> Response {
    match state
        .sso
        .begin_login(&provider, q.return_to.as_deref().unwrap_or("/"))
        .await
    {
        Ok(begun) => see_other(
            &begun.location,
            Some(set_cookie(&state, &begun.cookie, COOKIE_MAX_AGE)),
        ),
        Err(e) => browser_error(e),
    }
}

async fn callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<Callback>,
    request: Request<Body>,
) -> Response {
    let client = Client::of(&state, &request);
    let cookie = read_cookie(&state, request.headers());
    match state
        .sso
        .complete(&provider, &q, cookie.as_deref(), client.meta())
        .await
    {
        Ok(landed) => see_other(&landed.location, None),
        Err(e) => browser_error(e),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn exchange(State(state): State<AppState>, request: Request<Body>) -> Response {
    if let Some(r) = not_json(request.headers()) {
        return r;
    }
    let client = Client::of(&state, &request);
    let cookie = read_cookie(&state, request.headers());
    let Ok(body) = body_json::<CodeBody>(request).await else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid body"})),
        )
            .into_response();
    };
    match state
        .sso
        .exchange(&body.code, cookie.as_deref(), client.meta())
        .await
    {
        Ok(session) => {
            let mut response = Json(json!({
                "token": session.token,
                "username": session.username,
                "expires_at": crate::wire::wire_ts(session.expires_at),
                "return_to": session.return_to,
            }))
            .into_response();
            let headers = response.headers_mut();
            headers.insert(header::SET_COOKIE, set_cookie(&state, "", 0));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(e) => json_error(e),
    }
}

async fn body_json<T: serde::de::DeserializeOwned>(request: Request<Body>) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Deserialize)]
struct LinkStart {
    provider: String,
    password: String,
}

async fn link_start(State(state): State<AppState>, request: Request<Body>) -> AppResult<Response> {
    let caller = require_auth(&request)?;
    if let Some(r) = not_json(request.headers()) {
        return Ok(r);
    }
    let user_id = caller
        .user_id
        .ok_or_else(|| AppError::Forbidden("a static token has no account to link".into()))?;
    let body: LinkStart = body_json(request).await?;
    Ok(
        match state
            .sso
            .begin_link(user_id, &body.password, &body.provider)
            .await
        {
            Ok(begun) => {
                let mut response = Json(json!({ "location": begun.location })).into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    set_cookie(&state, &begun.cookie, COOKIE_MAX_AGE),
                );
                response
            }
            Err(e) => json_error(e),
        },
    )
}

#[derive(Deserialize)]
struct PendingQuery {
    code: String,
}

async fn link_pending(
    State(state): State<AppState>,
    Query(q): Query<PendingQuery>,
    request: Request<Body>,
) -> AppResult<Response> {
    let caller = require_auth(&request)?;
    let cookie = read_cookie(&state, request.headers());
    Ok(
        match state.sso.link_details(&q.code, cookie.as_deref()).await {
            Ok(p) if p.target == caller.username => Json(p).into_response(),
            Ok(_) => json_error(SsoError::Refused(SsoRefusal::WrongUser)),
            Err(e) => json_error(e),
        },
    )
}

async fn link_confirm(
    State(state): State<AppState>,
    request: Request<Body>,
) -> AppResult<Response> {
    let caller = require_auth(&request)?;
    if let Some(r) = not_json(request.headers()) {
        return Ok(r);
    }
    let user_id = caller
        .user_id
        .ok_or_else(|| AppError::Forbidden("a static token has no account to link".into()))?;
    let client = Client::of(&state, &request);
    let cookie = read_cookie(&state, request.headers());
    let body: CodeBody = body_json(request).await?;
    Ok(
        match state
            .sso
            .confirm_link(user_id, &body.code, cookie.as_deref(), client.meta())
            .await
        {
            Ok(()) => {
                let mut response = StatusCode::NO_CONTENT.into_response();
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, set_cookie(&state, "", 0));
                response
            }
            Err(e) => json_error(e),
        },
    )
}

async fn logout(State(state): State<AppState>, request: Request<Body>) -> AppResult<Response> {
    let caller = require_auth(&request)?;
    let token = state
        .auth
        .authenticate
        .live_api_token(&caller.token)
        .await?;
    Ok(
        match state
            .sso
            .logout(token.as_ref().map(|t| t.id.as_str()))
            .await
        {
            Ok(end) => Json(json!({ "end_session_url": end })).into_response(),
            Err(e) => json_error(e),
        },
    )
}

async fn target_user(state: &AppState, username: &str) -> AppResult<crate::domain::User> {
    state
        .users
        .by_name(username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))
}

async fn list_identities(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: Request<Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;
    let user = target_user(&state, &username).await?;
    let links = state.identities.of_user(user.id).await?;
    let disabled = state.identities.login_state(user.id).await?.disabled;
    let links: Vec<_> = links
        .iter()
        .map(|l| {
            json!({
                "provider": l.key.authority.provider,
                "issuer": l.key.authority.issuer,
                "subject": l.key.subject,
                "email": l.email,
                "provisioned": l.provisioned,
                "disabled": l.disabled,
                "linked_at": crate::wire::wire_ts(l.linked_at),
                "last_login_at": crate::wire::wire_ts(l.last_login_at),
            })
        })
        .collect();
    Ok(Json(
        json!({ "identities": links, "user_disabled": disabled }),
    ))
}

#[derive(Deserialize)]
struct IdentityQuery {
    provider: String,
    issuer: String,
    subject: String,
}

impl IdentityQuery {
    fn key(&self) -> IdentityKey {
        IdentityKey {
            authority: Authority::new(&self.provider, &self.issuer),
            subject: self.subject.clone(),
        }
    }
}

fn sso_app_error(e: SsoError) -> AppError {
    match e {
        SsoError::Store(crate::error::StoreError::NotFound) => {
            AppError::NotFound("no such identity".into())
        }
        SsoError::Store(e) => e.into(),
        SsoError::Refused(r) => AppError::Forbidden(r.as_str().into()),
        SsoError::Credentials(_) => AppError::Unauthorized("bad credentials".into()),
    }
}

async fn unlink(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(q): Query<IdentityQuery>,
    request: Request<Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;
    let user = target_user(&state, &username).await?;
    state
        .sso
        .unlink(user.id, &q.key())
        .await
        .map_err(sso_app_error)?;
    record_audit(&state, &caller, "sso.unlink", Some(&username)).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn disable_identity(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(q): Query<IdentityQuery>,
    request: Request<Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    target_user(&state, &username).await?;
    state
        .sso
        .disable_link(&q.key())
        .await
        .map_err(sso_app_error)?;
    record_audit(&state, &caller, "sso.identity.disable", Some(&username)).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn disable_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: Request<Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let user = target_user(&state, &username).await?;
    state.sso.disable_user(&user).await.map_err(sso_app_error)?;
    record_audit(&state, &caller, "user.disable", Some(&username)).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn enable_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: Request<Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let user = target_user(&state, &username).await?;
    state.sso.enable_user(&user).await.map_err(sso_app_error)?;
    record_audit(&state, &caller, "user.enable", Some(&username)).await;
    Ok(StatusCode::NO_CONTENT)
}
