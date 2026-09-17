use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{header, StatusCode};
use reqwest::{Client, RequestBuilder, Response, Url};
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::registry::resolve::{CacheRepo, Upstream};

const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(300);
const DOCKER_HUB_HOSTS: [&str; 3] = ["registry-1.docker.io", "index.docker.io", "docker.io"];
const DOCKER_HUB_REALM: &str = "https://auth.docker.io/token";

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UpstreamAuth {
    Basic { username: String, password: String },
    Bearer { token: String },
}

impl UpstreamAuth {
    /// `basic:user:pass` or `bearer:token`, the `OPENCARGO_UPSTREAM_AUTH_*` shape.
    pub fn parse_env(value: &str) -> anyhow::Result<Self> {
        let mut parts = value.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("basic"), Some(username), Some(password)) => Ok(Self::Basic {
                username: username.to_string(),
                password: password.to_string(),
            }),
            (Some("bearer"), Some(token), rest) => Ok(Self::Bearer {
                token: rest.map_or_else(|| token.to_string(), |r| format!("{token}:{r}")),
            }),
            _ => anyhow::bail!("upstream auth must be 'basic:user:pass' or 'bearer:token'"),
        }
    }

    fn apply(&self, req: RequestBuilder) -> RequestBuilder {
        match self {
            Self::Basic { username, password } => req.basic_auth(username, Some(password)),
            Self::Bearer { token } => req.bearer_auth(token),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpstreamCreds {
    pub auth: Option<UpstreamAuth>,
    pub token_realms: Vec<Url>,
    pub dl_allow_private: bool,
}

pub fn is_docker_hub_host(host: &str) -> bool {
    DOCKER_HUB_HOSTS.contains(&host)
}

/// Hub's token realm sits on another host than its registry, so credentials
/// would otherwise never reach it.
pub fn default_token_realms(base: &Url) -> Vec<Url> {
    match base.host_str() {
        Some(host) if is_docker_hub_host(host) => {
            vec![Url::parse(DOCKER_HUB_REALM).expect("static realm URL")]
        }
        _ => Vec::new(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BearerChallenge {
    pub realm: Url,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// RFC 6750 `Bearer realm="..",service="..",scope=".."`; params in any order,
/// quoted or bare. Anything but a Bearer challenge with a parseable realm is `None`.
pub fn parse_bearer_challenge(www_authenticate: &str) -> Option<BearerChallenge> {
    let rest = www_authenticate.trim();
    let (scheme, params) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for (name, value) in split_params(params) {
        match name.to_ascii_lowercase().as_str() {
            "realm" => realm = Some(value),
            "service" => service = Some(value),
            "scope" => scope = Some(value),
            _ => {}
        }
    }
    Some(BearerChallenge {
        realm: Url::parse(&realm?).ok()?,
        service,
        scope,
    })
}

fn split_params(params: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = params;
    while let Some((name, tail)) = rest.split_once('=') {
        let tail = tail.trim_start();
        let (value, after) = match tail.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => tail.split_once(',').unwrap_or((tail, "")),
        };
        out.push((name.trim().to_string(), value.trim().to_string()));
        rest = after.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

/// The member holding the credentials, the upstream base and the scope: a
/// token embodies the credentials it was acquired with, so two proxies on
/// one upstream never share one.
type TokenKey = (i64, String, String);

/// The realm is only known once a challenge arrived, and the point of the
/// cache is to skip it.
#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<HashMap<TokenKey, (String, Instant)>>,
}

impl TokenCache {
    fn get(&self, member_id: i64, base: &Url, scope: &str) -> Option<String> {
        let cache = self.inner.lock().expect("token cache poisoned");
        cache
            .get(&(member_id, base.to_string(), scope.to_string()))
            .filter(|(_, until)| *until > Instant::now())
            .map(|(token, _)| token.clone())
    }

    fn put(&self, member_id: i64, base: &Url, scope: &str, token: String, ttl: Duration) {
        let mut cache = self.inner.lock().expect("token cache poisoned");
        cache.insert(
            (member_id, base.to_string(), scope.to_string()),
            (token, Instant::now() + ttl.mul_f32(0.9)),
        );
    }
}

#[derive(serde::Deserialize)]
struct TokenReply {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

/// Attach a cached token or the static auth; on a Bearer challenge with a
/// strategy scope, acquire a token and retry once.
pub(crate) async fn send_with_auth(
    http: &Client,
    cache: &TokenCache,
    member: CacheRepo<'_>,
    up: &Upstream,
    req: RequestBuilder,
    scope: Option<&str>,
) -> AppResult<Response> {
    let retry = req
        .try_clone()
        .ok_or_else(|| AppError::Internal("upstream request is not replayable".into()))?;
    let first = match scope.and_then(|s| cache.get(member.0.id, &up.base, s)) {
        Some(token) => req.bearer_auth(token),
        None => up.auth.iter().fold(req, |r, a| a.apply(r)),
    };
    let resp = first.send().await.map_err(transport)?;
    if resp.status() != StatusCode::UNAUTHORIZED {
        return Ok(resp);
    }
    let scope = scope.ok_or_else(|| {
        AppError::BadGateway("upstream answered 401 for an artifact without bearer scope".into())
    })?;
    let challenge = resp
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer_challenge)
        .ok_or_else(|| {
            AppError::BadGateway("upstream answered 401 without a Bearer challenge".into())
        })?;
    let challenge = BearerChallenge {
        scope: challenge.scope.or_else(|| Some(scope.to_string())),
        ..challenge
    };
    let token = acquire_token(http, cache, member, &challenge, up).await?;
    retry.bearer_auth(token).send().await.map_err(transport)
}

pub(crate) async fn acquire_token(
    http: &Client,
    cache: &TokenCache,
    member: CacheRepo<'_>,
    ch: &BearerChallenge,
    up: &Upstream,
) -> AppResult<String> {
    check_realm(&ch.realm, up).await?;
    let mut req = http.get(ch.realm.clone());
    if let Some(service) = &ch.service {
        req = req.query(&[("service", service)]);
    }
    if let Some(scope) = &ch.scope {
        req = req.query(&[("scope", scope)]);
    }
    if realm_may_see_credentials(&ch.realm, up) {
        req = up.auth.iter().fold(req, |r, a| a.apply(r));
    }
    let resp = req.send().await.map_err(transport)?;
    if !resp.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "token realm {} refused a token: {}",
            ch.realm,
            resp.status()
        )));
    }
    let reply: TokenReply = resp
        .json()
        .await
        .map_err(|e| AppError::BadGateway(format!("token realm returned an invalid body: {e}")))?;
    let token = reply
        .token
        .or(reply.access_token)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::BadGateway("token realm returned no token".into()))?;
    let ttl = reply
        .expires_in
        .map_or(DEFAULT_TOKEN_TTL, Duration::from_secs);
    if let Some(scope) = &ch.scope {
        cache.put(member.0.id, &up.base, scope, token.clone(), ttl);
    }
    Ok(token)
}

/// The realm is chosen by the upstream: only the admin's own endpoints (the
/// upstream, `token_realms`, or any with `dl_allow_private`) may be private.
async fn check_realm(realm: &Url, up: &Upstream) -> AppResult<()> {
    let refused = |e: AppError| AppError::BadGateway(format!("upstream token realm refused: {e}"));
    super::validate_upstream_url(realm.as_str()).map_err(refused)?;
    if up.dl_allow_private || admin_endpoint(realm, up) {
        return Ok(());
    }
    super::refuse_blocked_host(realm).await.map_err(refused)
}

fn admin_endpoint(realm: &Url, up: &Upstream) -> bool {
    super::same_endpoint(realm, &up.base) || up.token_realms.iter().any(|r| r == realm)
}

// A hostile realm would otherwise collect the upstream credentials.
fn realm_may_see_credentials(realm: &Url, up: &Upstream) -> bool {
    if admin_endpoint(realm, up) {
        return true;
    }
    warn!(realm = %realm, upstream = %up.base, "token realm is off the upstream host; querying it anonymously");
    false
}

fn transport(e: reqwest::Error) -> AppError {
    AppError::BadGateway(format!("upstream request failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bearer_challenge_variants() {
        let full = parse_bearer_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        )
        .unwrap();
        assert_eq!(full.realm.as_str(), "https://auth.docker.io/token");
        assert_eq!(full.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(
            full.scope.as_deref(),
            Some("repository:library/alpine:pull")
        );

        let reordered = parse_bearer_challenge(
            r#"bearer scope="repository:a/b:pull", service=ghcr.io , realm=https://ghcr.io/token"#,
        )
        .unwrap();
        assert_eq!(reordered.realm.as_str(), "https://ghcr.io/token");
        assert_eq!(reordered.service.as_deref(), Some("ghcr.io"));
        assert_eq!(reordered.scope.as_deref(), Some("repository:a/b:pull"));

        let bare = parse_bearer_challenge("Bearer realm=\"http://127.0.0.1:5000/token\"").unwrap();
        assert_eq!(bare.realm.as_str(), "http://127.0.0.1:5000/token");
        assert_eq!((bare.service, bare.scope), (None, None));

        let extra = parse_bearer_challenge(
            r#"Bearer error="invalid_token",realm="https://r.example/t",error_description="x, y""#,
        )
        .unwrap();
        assert_eq!(extra.realm.as_str(), "https://r.example/t");

        assert!(parse_bearer_challenge(r#"Basic realm="opencargo""#).is_none());
        assert!(parse_bearer_challenge(r#"Bearer service="x""#).is_none());
        assert!(parse_bearer_challenge(r#"Bearer realm="not a url""#).is_none());
        assert!(parse_bearer_challenge("").is_none());

        assert!(matches!(
            UpstreamAuth::parse_env("basic:u:p:with:colons").unwrap(),
            UpstreamAuth::Basic { username, password } if username == "u" && password == "p:with:colons"
        ));
        assert!(matches!(
            UpstreamAuth::parse_env("bearer:tok:en").unwrap(),
            UpstreamAuth::Bearer { token } if token == "tok:en"
        ));
        assert!(UpstreamAuth::parse_env("basic:only-user").is_err());
        assert!(UpstreamAuth::parse_env("digest:x").is_err());

        let hub = Url::parse("https://registry-1.docker.io").unwrap();
        assert_eq!(default_token_realms(&hub)[0].as_str(), DOCKER_HUB_REALM);
        let other = Url::parse("https://ghcr.io").unwrap();
        assert!(default_token_realms(&other).is_empty());
    }
}
