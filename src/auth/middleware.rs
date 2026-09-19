use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use serde_json::json;

use crate::app::authenticate::{Authenticate, Credential, Presented, Refusal, Transport};
use crate::error::StoreError;

pub use crate::app::authenticate::AuthUser;

/// What a protocol adapter declares about the routes it owns: its
/// challenge, its 401 body, its anonymous-read exemptions and the extra
/// credential headers it reads. `src/auth` itself knows no route.
pub trait RouteRules: Send + Sync {
    fn owns(&self, path: &str) -> bool;

    fn challenge(&self, _method: &Method, _path: &str) -> Option<HeaderValue> {
        None
    }

    fn unauthorized(&self) -> Response {
        unauthorized_response()
    }

    fn anonymous_exempt(&self, _method: &Method, _path: &str) -> bool {
        false
    }

    fn accepts_registry_tokens(&self) -> bool {
        false
    }

    fn extra_credentials(&self, _headers: &HeaderMap) -> Vec<Presented> {
        Vec::new()
    }

    fn primary(&self, _method: &Method, _path: &str) -> Option<Transport> {
        None
    }
}

#[derive(Clone)]
pub struct AuthState {
    pub anonymous_read: bool,
    pub authenticate: Arc<Authenticate>,
    pub routes: Vec<Arc<dyn RouteRules>>,
    /// Peers whose `X-Forwarded-For` names the client; nobody by default.
    pub trusted_proxies: Vec<IpAddr>,
}

impl AuthState {
    fn route(&self, path: &str) -> Option<&dyn RouteRules> {
        self.routes.iter().find(|r| r.owns(path)).map(|r| r.as_ref())
    }
}

/// Every request goes through [`Authenticate`]; a credential presented that
/// does not verify is a 401 whatever its scheme, anonymous is reserved for a
/// request that presented none.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let route = state.route(&path);
    let challenge = route.and_then(|r| r.challenge(&method, &path));
    let unauthorized = || route.map_or_else(unauthorized_response, |r| r.unauthorized());

    let presented = match presented(request.headers(), route) {
        Ok(presented) => presented,
        Err(()) => return with_challenge(unauthorized(), challenge),
    };
    let source = client_source(&request, &state.trusted_proxies);
    let primary = route.and_then(|r| r.primary(&method, &path));
    let response = match state.authenticate.run(&presented, primary, &source).await {
        Ok(Some(done)) => {
            if let Some(claims) = done.claims {
                request.extensions_mut().insert(claims);
            }
            match done.user {
                Some(user) => run_as(user, request, next).await,
                None => run_anonymous(&state, route, request, next).await,
            }
        }
        Ok(None) if is_read(&method) && route.is_some_and(|r| r.anonymous_exempt(&method, &path)) => {
            next.run(request).await
        }
        Ok(None) => run_anonymous(&state, route, request, next).await,
        Err(Refusal::Invalid) => unauthorized(),
        Err(Refusal::Throttled) => too_many_requests_response(),
        Err(Refusal::Unavailable) => service_unavailable_response(),
    };
    with_challenge(response, challenge)
}

fn with_challenge(mut response: Response, challenge: Option<HeaderValue>) -> Response {
    if let Some(challenge) = challenge.filter(|_| response.status() == StatusCode::UNAUTHORIZED) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

/// Every credential the request carries; `Err` for a registry token on a
/// route that takes none. A scheme this server does not speak is no
/// credential, as it always was.
fn presented(headers: &HeaderMap, route: Option<&dyn RouteRules>) -> Result<Vec<Presented>, ()> {
    let mut all = Vec::new();
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(authorization_credential);
    if let Some(credential) = authorization {
        if matches!(credential, Credential::Registry(_))
            && !route.is_some_and(|r| r.accepts_registry_tokens())
        {
            return Err(());
        }
        all.push(Presented {
            transport: Transport::Authorization,
            credential,
        });
    }
    if let Some(route) = route {
        all.extend(route.extra_credentials(headers));
    }
    Ok(all)
}

/// The credential of an `Authorization` value: Basic, a registry token, or
/// any other Bearer. An unknown scheme is not a credential.
pub(crate) fn authorization_credential(value: &str) -> Option<Credential> {
    if let Some(raw) = value.strip_prefix("Bearer ") {
        if raw.starts_with(crate::registry::oci::token::PREFIX) {
            return Some(Credential::Registry(raw.to_string()));
        }
        return Some(Credential::Bearer(raw.to_string()));
    }
    basic_credentials(value).map(|(username, password)| Credential::Basic { username, password })
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

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The peer address, or the address a configured trusted proxy forwarded;
/// `X-Forwarded-For` from anyone else is ignored.
pub fn client_source<B>(request: &Request<B>, trusted: &[IpAddr]) -> String {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    source_of(peer, request.headers(), trusted)
}

pub(crate) fn source_of(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &[IpAddr]) -> String {
    let Some(peer) = peer else {
        return "unknown".to_string();
    };
    if !trusted.contains(&peer) {
        return peer.to_string();
    }
    headers
        .get(X_FORWARDED_FOR)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
        .to_string()
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

async fn run_anonymous(
    state: &AuthState,
    route: Option<&dyn RouteRules>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if state.anonymous_read && is_read(request.method()) {
        next.run(request).await
    } else {
        route.map_or_else(unauthorized_response, |r| r.unauthorized())
    }
}

/// A CORS preflight is a read: no body, no effect, the browser asking.
fn is_read(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD || method == Method::OPTIONS
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Invalid or missing authentication token"
        })),
    )
        .into_response()
}

fn too_many_requests_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({"error": "too many authentication attempts, try again later"})),
    )
        .into_response()
}

/// Retryable, unlike the 401 a store failure used to be collapsed into.
fn service_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "authentication temporarily unavailable, try again"})),
    )
        .into_response()
}

/// While `must_change_password` is set, allow only the password-change endpoint
/// (`PUT .../password`) and `/-/whoami`.
fn password_change_pending_block(
    auth_user: &AuthUser,
    method: &Method,
    path: &str,
) -> Option<Response> {
    if !auth_user.must_change_password {
        return None;
    }
    let allowed = (method == Method::PUT && path.ends_with("/password")) || path == "/-/whoami";
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

/// A Bearer value through [`Authenticate`], for the callers outside the
/// middleware (the WebSocket's first frame). `Ok(None)` is a rejection,
/// `Err` a store that could not answer.
pub(crate) async fn authenticate_bearer(
    state: &AuthState,
    token: &str,
    source: &str,
) -> Result<Option<AuthUser>, StoreError> {
    let presented = [Presented {
        transport: Transport::Authorization,
        credential: Credential::Bearer(token.to_string()),
    }];
    match state.authenticate.run(&presented, None, source).await {
        Ok(done) => Ok(done.and_then(|d| d.user)),
        Err(Refusal::Unavailable) => Err(StoreError::Unavailable),
        Err(Refusal::Invalid | Refusal::Throttled) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn forwarded_for_is_ignored_without_a_trusted_proxy() {
        let peer = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("203.0.113.9"));
        assert_eq!(source_of(Some(peer), &headers, &[]), "10.0.0.7");
        assert_eq!(source_of(Some(peer), &headers, &[peer]), "203.0.113.9");
        headers.insert(X_FORWARDED_FOR, HeaderValue::from_static("1.1.1.1, 203.0.113.9"));
        assert_eq!(
            source_of(Some(peer), &headers, &[peer]),
            "203.0.113.9",
            "the address the trusted proxy appended"
        );
        assert_eq!(source_of(None, &headers, &[peer]), "unknown");
    }

    #[test]
    fn an_unreadable_or_foreign_scheme_is_no_credential() {
        assert!(authorization_credential("Digest abc").is_none());
        assert!(matches!(
            authorization_credential("Bearer ocr_x.y"),
            Some(Credential::Registry(_))
        ));
        assert!(matches!(
            authorization_credential("Bearer trg_abc"),
            Some(Credential::Bearer(_))
        ));
    }
}
