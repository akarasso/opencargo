//! The source-side gate: the only place in the importer that owns a client
//! for the source. Every request goes through [`Gate::send`], which applies
//! the reachability tests, the credential's host scope, the throttle, the
//! retries and the redaction of every error.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use sha1::Digest as _;
use tokio::io::AsyncWriteExt;

use crate::ports::import::{redact, CopyError, Digests, SourceError};
use crate::proxy::auth::parse_bearer_challenge;
use crate::proxy::{is_blocked_ip, refuse_blocked_host, same_endpoint};

/// A credential the importer holds: it never prints, and it cannot be
/// serialized into the run's options.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[derive(Clone, Debug, Default)]
pub enum Credential {
    #[default]
    Anonymous,
    Basic { user: String, password: Secret },
    Bearer(Secret),
}

impl Credential {
    fn header(&self) -> Option<HeaderValue> {
        let raw = match self {
            Credential::Anonymous => return None,
            Credential::Basic { user, password } => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{}", password.expose()))
            ),
            Credential::Bearer(t) => format!("Bearer {}", t.expose()),
        };
        let mut v = HeaderValue::from_str(&raw).ok()?;
        v.set_sensitive(true);
        Some(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Auth(String),
    #[error("{size} bytes over the {limit}-byte limit")]
    TooLarge { size: u64, limit: u64 },
    #[error("{0}")]
    Checksum(String),
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
}

impl From<FetchError> for CopyError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::TooLarge { size, limit } => CopyError::TooLarge { size, limit },
            FetchError::Transient(m) => CopyError::Transient(m),
            FetchError::Refused(m) => CopyError::Permanent(format!("refused: {m}")),
            other => CopyError::Permanent(format!("source: {other}")),
        }
    }
}

impl From<FetchError> for SourceError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Auth(m) => SourceError::Auth(m),
            FetchError::Refused(m) => SourceError::Refused(m),
            other => SourceError::Unavailable(other.to_string()),
        }
    }
}

/// Hosts matched as `host` (any port) or `host:port`.
#[derive(Clone, Debug, Default)]
pub struct HostSet(HashSet<String>);

impl HostSet {
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Self(entries.into_iter().map(|e| e.to_ascii_lowercase()).collect())
    }

    pub fn contains(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else { return false };
        let host = host.to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(0);
        self.0.contains(&host) || self.0.contains(&format!("{host}:{port}"))
    }
}

fn hop_blocked(url: &Url) -> bool {
    use std::net::ToSocketAddrs;
    let Some(host) = url.host_str() else { return true };
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        return is_blocked_ip(&ip);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match (host, port).to_socket_addrs() {
        Ok(mut addrs) => addrs.any(|a| is_blocked_ip(&a.ip())),
        Err(_) => true,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("redirect to {0} refused: a private address, not named by --allow-source-host")]
struct RefusedHop(String);

/// Token bucket per host, plus the cooldown a 429 or 503 imposes on it.
#[derive(Default)]
pub struct Throttle {
    interval: Duration,
    next: Mutex<HashMap<String, Instant>>,
}

impl Throttle {
    pub fn new(rate_per_sec: f64) -> Self {
        let interval = if rate_per_sec > 0.0 {
            Duration::from_secs_f64(1.0 / rate_per_sec)
        } else {
            Duration::ZERO
        };
        Self { interval, next: Mutex::new(HashMap::new()) }
    }

    pub async fn acquire(&self, host: &str) {
        let wait = {
            let mut next = self.next.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            let slot = next.get(host).copied().filter(|t| *t > now).unwrap_or(now);
            next.insert(host.to_string(), slot + self.interval);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub fn cool(&self, host: &str, for_: Duration) {
        let mut next = self.next.lock().unwrap_or_else(|p| p.into_inner());
        let until = Instant::now() + for_;
        let slot = next.entry(host.to_string()).or_insert(until);
        if *slot < until {
            *slot = until;
        }
    }
}

pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// The `rel="next"` target of an RFC 5988 `Link` header.
pub fn link_next(headers: &HeaderMap, base: &Url) -> Option<Url> {
    for value in headers.get_all(reqwest::header::LINK) {
        let Ok(value) = value.to_str() else { continue };
        for part in value.split(',') {
            let mut pieces = part.split(';');
            let target = pieces.next()?.trim();
            let is_next = pieces.any(|p| {
                let p = p.trim().replace(' ', "");
                p == "rel=\"next\"" || p == "rel=next"
            });
            if is_next {
                let target = target.trim_start_matches('<').trim_end_matches('>');
                return base.join(target).ok();
            }
        }
    }
    None
}

pub struct GateConfig {
    /// `--from`: reachable and in the credential's scope.
    pub from: Url,
    pub credential: Credential,
    pub allow_hosts: HostSet,
    pub rate: f64,
    pub retries: u32,
    pub max_backoff: Duration,
    pub max_artifact_size: u64,
    pub spool_dir: PathBuf,
    pub timeout: Duration,
}

type Observer = Arc<dyn Fn(&Method, &Url, bool) + Send + Sync>;

pub struct Gate {
    client: reqwest::Client,
    from: Url,
    scope: Mutex<Vec<Url>>,
    credential: Credential,
    allow_hosts: HostSet,
    throttle: Throttle,
    retries: u32,
    max_backoff: Duration,
    max_artifact_size: u64,
    spool_dir: PathBuf,
    bearer: Mutex<HashMap<String, Secret>>,
    observer: Option<Observer>,
}

/// A request the gate sends: the body is cloned per attempt.
#[derive(Clone)]
pub struct Req {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Option<Bytes>,
}

impl Req {
    pub fn get(url: Url) -> Self {
        Self { method: Method::GET, url, headers: Vec::new(), body: None }
    }
    pub fn head(url: Url) -> Self {
        Self { method: Method::HEAD, url, headers: Vec::new(), body: None }
    }
    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        if let Ok(v) = HeaderValue::from_str(value) {
            self.headers.push((HeaderName::from_static(name), v));
        }
        self
    }
    pub fn body(mut self, method: Method, body: impl Into<Bytes>) -> Self {
        self.method = method;
        self.body = Some(body.into());
        self
    }
}

pub struct Spooled {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub sha1: String,
    pub sha512: Vec<u8>,
    /// No algorithm the source offered could be checked.
    pub unverified: bool,
}

impl Spooled {
    pub fn integrity(&self) -> String {
        format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(&self.sha512))
    }

    pub async fn bytes(&self) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(&self.path).await
    }

    pub async fn body(&self) -> std::io::Result<reqwest::Body> {
        let f = tokio::fs::File::open(&self.path).await?;
        Ok(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f)))
    }
}

impl Drop for Spooled {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn transport(url: &Url, e: reqwest::Error) -> FetchError {
    let mut chain = String::new();
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
    while let Some(s) = src {
        if let Some(hop) = s.downcast_ref::<RefusedHop>() {
            return FetchError::Refused(hop.to_string());
        }
        chain = s.to_string();
        src = s.source();
    }
    let e = e.without_url();
    FetchError::Transient(format!("{}: {e}{}", redact(url), if chain.is_empty() { String::new() } else { format!(" ({chain})") }))
}

impl Gate {
    pub fn new(cfg: GateConfig) -> Result<Self, String> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let allow = cfg.allow_hosts.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            let origin = attempt.previous().last();
            let url = attempt.url().clone();
            if origin.is_some_and(|o| same_endpoint(o, &url)) || allow.contains(&url) || !hop_blocked(&url) {
                return attempt.follow();
            }
            attempt.error(RefusedHop(url.host_str().unwrap_or("?").to_string()))
        });
        let client = reqwest::Client::builder()
            .redirect(policy)
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("opencargo-import/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&cfg.spool_dir).map_err(|e| format!("{}: {e}", cfg.spool_dir.display()))?;
        sweep_spool(&cfg.spool_dir);
        Ok(Self {
            client,
            scope: Mutex::new(vec![cfg.from.clone()]),
            from: cfg.from,
            credential: cfg.credential,
            allow_hosts: cfg.allow_hosts,
            throttle: Throttle::new(cfg.rate),
            retries: cfg.retries,
            max_backoff: cfg.max_backoff,
            max_artifact_size: cfg.max_artifact_size,
            spool_dir: cfg.spool_dir,
            bearer: Mutex::new(HashMap::new()),
            observer: None,
        })
    }

    /// Records every request the gate sends: `(method, url, credentialed)`.
    pub fn observe(mut self, f: impl Fn(&Method, &Url, bool) + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(f));
        self
    }

    pub fn from(&self) -> &Url {
        &self.from
    }

    /// An endpoint the adapter names in its own code, reached and
    /// credentialed like `--from`.
    pub fn name_endpoint(&self, url: Url) {
        self.scope.lock().unwrap_or_else(|p| p.into_inner()).push(url);
    }

    pub fn max_artifact_size(&self) -> u64 {
        self.max_artifact_size
    }

    fn in_scope(&self, url: &Url) -> bool {
        self.scope.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|s| same_endpoint(s, url))
    }

    async fn admit(&self, url: &Url) -> Result<(), FetchError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(FetchError::Refused(format!("{} is not http(s)", redact(url))));
        }
        if self.in_scope(url) || self.allow_hosts.contains(url) {
            return Ok(());
        }
        refuse_blocked_host(url).await.map_err(|_| {
            FetchError::Refused(format!(
                "{} is a private address the source named; allow it with --allow-source-host {}",
                redact(url),
                url.host_str().unwrap_or("?")
            ))
        })
    }

    async fn bearer_for(&self, url: &Url, challenge: &str) -> Result<Option<Secret>, FetchError> {
        let Some(c) = parse_bearer_challenge(challenge) else { return Ok(None) };
        let key = format!("{}|{}", c.realm, c.scope.clone().unwrap_or_default());
        if let Some(t) = self.bearer.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
            return Ok(Some(t.clone()));
        }
        let mut realm = c.realm.clone();
        {
            let mut q = realm.query_pairs_mut();
            if let Some(s) = &c.service {
                q.append_pair("service", s);
            }
            if let Some(s) = &c.scope {
                q.append_pair("scope", s);
            }
        }
        self.admit(&realm).await?;
        let mut req = self.client.get(realm.clone());
        let credentialed = self.in_scope(&realm) || same_endpoint(&realm, url) && self.in_scope(url);
        if credentialed {
            if let Some(h) = self.credential.header() {
                req = req.header(reqwest::header::AUTHORIZATION, h);
            }
        }
        if let Some(obs) = &self.observer {
            obs(&Method::GET, &realm, credentialed);
        }
        let resp = req.send().await.map_err(|e| transport(&realm, e))?;
        if !resp.status().is_success() {
            return Err(FetchError::Auth(format!("token realm {} answered {}", redact(&realm), resp.status())));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| transport(&realm, e))?;
        let token = body
            .get("token")
            .or_else(|| body.get("access_token"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| FetchError::Auth(format!("token realm {} returned no token", redact(&realm))))?;
        let secret = Secret::new(token);
        self.bearer.lock().unwrap_or_else(|p| p.into_inner()).insert(key, secret.clone());
        Ok(Some(secret))
    }

    /// Sends with the gate's tests, throttle and retries. Non-success
    /// statuses other than 401/403/404/429/5xx are returned to the caller.
    pub async fn send(&self, r: &Req) -> Result<reqwest::Response, FetchError> {
        self.admit(&r.url).await?;
        let host = r.url.host_str().unwrap_or("").to_string();
        let credentialed = self.in_scope(&r.url);
        let mut bearer: Option<Secret> = None;
        let mut attempt = 0u32;
        loop {
            self.throttle.acquire(&host).await;
            let mut req = self.client.request(r.method.clone(), r.url.clone());
            for (n, v) in &r.headers {
                req = req.header(n, v);
            }
            if let Some(b) = &r.body {
                req = req.body(b.clone());
            }
            let sent_auth = match (&bearer, credentialed) {
                (Some(t), _) => {
                    req = req.bearer_auth(t.expose());
                    true
                }
                (None, true) => match self.credential.header() {
                    Some(h) => {
                        req = req.header(reqwest::header::AUTHORIZATION, h);
                        true
                    }
                    None => false,
                },
                (None, false) => false,
            };
            if let Some(obs) = &self.observer {
                obs(&r.method, &r.url, sent_auth);
            }
            let outcome = req.send().await;
            let resp = match outcome {
                Ok(resp) => resp,
                Err(e) => {
                    let err = transport(&r.url, e);
                    if matches!(err, FetchError::Transient(_)) && attempt < self.retries {
                        attempt += 1;
                        self.backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
            };
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && bearer.is_none() {
                if let Some(ch) = resp.headers().get(reqwest::header::WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()) {
                    if ch.to_ascii_lowercase().starts_with("bearer") {
                        if let Some(t) = self.bearer_for(&r.url, ch).await? {
                            bearer = Some(t);
                            continue;
                        }
                    }
                }
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(if credentialed {
                    FetchError::Auth(format!("{} answered {status}", redact(&r.url)))
                } else {
                    FetchError::Refused(format!(
                        "{} answered {status} to an anonymous request: credentials are never sent outside the source's own endpoints",
                        host
                    ))
                });
            }
            if status == StatusCode::NOT_FOUND {
                return Err(FetchError::NotFound(redact(&r.url)));
            }
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                if attempt < self.retries {
                    attempt += 1;
                    let wait = retry_after(resp.headers()).unwrap_or_else(|| self.jitter(attempt)).min(self.max_backoff);
                    self.throttle.cool(&host, wait);
                    continue;
                }
                return Err(FetchError::Transient(format!("{} answered {status}", redact(&r.url))));
            }
            return Ok(resp);
        }
    }

    fn jitter(&self, attempt: u32) -> Duration {
        use rand::Rng;
        let base = 200u64.saturating_mul(1 << attempt.min(8));
        Duration::from_millis(base + rand::thread_rng().gen_range(0..=base / 2)).min(self.max_backoff)
    }

    async fn backoff(&self, attempt: u32) {
        tokio::time::sleep(self.jitter(attempt)).await;
    }

    pub async fn ok(&self, r: &Req) -> Result<reqwest::Response, FetchError> {
        let resp = self.send(r).await?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(FetchError::Permanent(format!("{} answered {}", redact(&r.url), resp.status())))
        }
    }

    pub async fn bytes(&self, r: &Req, max: u64) -> Result<Bytes, FetchError> {
        let resp = self.ok(r).await?;
        read_capped(resp, max, &r.url).await
    }

    pub async fn json(&self, r: &Req, max: u64) -> Result<serde_json::Value, FetchError> {
        let body = self.bytes(r, max).await?;
        serde_json::from_slice(&body)
            .map_err(|e| FetchError::Permanent(format!("{} is not JSON: {e}", redact(&r.url))))
    }

    pub async fn get_json(&self, url: Url) -> Result<serde_json::Value, FetchError> {
        self.json(&Req::get(url).header("accept", "application/json"), 32 << 20).await
    }

    /// Downloads to a spool file, hashing on the way, refusing past the size
    /// cap and verifying the strongest digest the source offered.
    pub async fn spool(&self, url: Url, want: &Digests) -> Result<Spooled, FetchError> {
        let resp = self.ok(&Req::get(url.clone())).await?;
        let limit = self.max_artifact_size;
        if let Some(len) = resp.content_length().filter(|l| *l > limit) {
            return Err(FetchError::TooLarge { size: len, limit });
        }
        let path = self.spool_dir.join(format!("{}.part-{}", uuid::Uuid::new_v4(), std::process::id()));
        let mut spooled = Spooled {
            path: path.clone(),
            size: 0,
            sha256: String::new(),
            sha1: String::new(),
            sha512: Vec::new(),
            unverified: false,
        };
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| FetchError::Permanent(format!("spool: {e}")))?;
        let (mut s256, mut s1, mut s512) = (sha2::Sha256::new(), sha1::Sha1::new(), sha2::Sha512::new());
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| transport(&url, e))?;
            spooled.size += chunk.len() as u64;
            if spooled.size > limit {
                return Err(FetchError::TooLarge { size: spooled.size, limit });
            }
            s256.update(&chunk);
            s1.update(&chunk);
            s512.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| FetchError::Permanent(format!("spool: {e}")))?;
        }
        file.flush().await.map_err(|e| FetchError::Permanent(format!("spool: {e}")))?;
        spooled.sha256 = hex(&s256.finalize());
        spooled.sha1 = hex(&s1.finalize());
        spooled.sha512 = s512.finalize().to_vec();
        verify(&spooled, want, &url)?;
        spooled.unverified = want.is_empty();
        Ok(spooled)
    }
}

fn verify(s: &Spooled, want: &Digests, url: &Url) -> Result<(), FetchError> {
    let mismatch = |alg: &str, want: &str, got: &str| {
        FetchError::Checksum(format!(
            "checksum mismatch on {}: {alg} {want} announced, {got} received",
            redact(url)
        ))
    };
    if let Some(sri) = &want.integrity {
        if let Some(b64) = sri.strip_prefix("sha512-") {
            let got = base64::engine::general_purpose::STANDARD.encode(&s.sha512);
            if got != b64 {
                return Err(mismatch("sha512", b64, &got));
            }
            return Ok(());
        }
    }
    if let Some(h) = &want.sha256 {
        let h = h.trim_start_matches("sha256:").to_ascii_lowercase();
        if h != s.sha256 {
            return Err(mismatch("sha256", &h, &s.sha256));
        }
        return Ok(());
    }
    if let Some(h) = &want.sha1 {
        let h = h.to_ascii_lowercase();
        if h != s.sha1 {
            return Err(mismatch("sha1", &h, &s.sha1));
        }
    }
    Ok(())
}

pub async fn read_capped(resp: reqwest::Response, max: u64, url: &Url) -> Result<Bytes, FetchError> {
    if let Some(len) = resp.content_length().filter(|l| *l > max) {
        return Err(FetchError::TooLarge { size: len, limit: max });
    }
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| transport(url, e))?;
        if (out.len() + chunk.len()) as u64 > max {
            return Err(FetchError::TooLarge { size: (out.len() + chunk.len()) as u64, limit: max });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

const STALE_SPOOL: Duration = Duration::from_secs(6 * 3600);

fn sweep_spool(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().contains(".part-") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > STALE_SPOOL);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Refuses the two admin-chosen URLs with credentials in them: those move to
/// environment variables, where no process listing shows them.
pub fn admin_url(raw: &str, flag: &str, env_hint: &str) -> Result<Url, String> {
    crate::proxy::validate_upstream_url(raw).map_err(|e| format!("{flag}: {e}"))?;
    let mut url = Url::parse(raw).map_err(|e| format!("{flag}: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{flag} carries credentials; pass them through {env_hint} instead, never on the command line"
        ));
    }
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header_rfc5988() {
        let base = Url::parse("https://reg.example/v2/app/tags/list").unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            HeaderValue::from_static("</v2/app/tags/list?n=2&last=b>; rel=\"next\", <https://x/>; rel=\"prev\""),
        );
        assert_eq!(link_next(&h, &base).unwrap().as_str(), "https://reg.example/v2/app/tags/list?n=2&last=b");
        assert!(link_next(&HeaderMap::new(), &base).is_none());
    }

    #[tokio::test]
    async fn throttle_respects_rate_and_cooldown() {
        let t = Throttle::new(1000.0);
        let start = Instant::now();
        for _ in 0..5 {
            t.acquire("h").await;
        }
        let before = Instant::now();
        t.cool("other", Duration::from_secs(30));
        t.acquire("h").await;
        assert!(start.elapsed() < Duration::from_secs(30), "a cooldown on one host never parks another");
        let slot = *t.next.lock().unwrap().get("other").unwrap();
        assert!(slot >= before + Duration::from_secs(30));
    }

    #[test]
    fn secrets_never_print() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s} {s:?}"), "*** Secret(***)");
        let c = Credential::Basic { user: "u".into(), password: s };
        assert!(!format!("{c:?}").contains("hunter2"));
    }

    #[test]
    fn admin_urls_with_userinfo_are_refused() {
        let err = admin_url("https://admin:hunter2@nexus.internal", "--from", "OPENCARGO_IMPORT_SOURCE_USER").unwrap_err();
        assert!(err.contains("OPENCARGO_IMPORT_SOURCE_USER") && !err.contains("hunter2"), "{err}");
        assert_eq!(admin_url("http://127.0.0.1:4873", "--from", "").unwrap().as_str(), "http://127.0.0.1:4873/");
    }

    type Seen = Arc<Mutex<Vec<(String, bool)>>>;

    async fn serve(router: axum::Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    fn recording(seen: Seen, body: &'static [u8]) -> axum::Router {
        axum::Router::new().fallback(move |req: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                let auth = req.headers().contains_key("authorization");
                seen.lock().unwrap().push((req.uri().path().to_string(), auth));
                body
            }
        })
    }

    fn gate(from: u16, allow: &[String], dir: &Path) -> Gate {
        Gate::new(GateConfig {
            from: Url::parse(&format!("http://127.0.0.1:{from}/")).unwrap(),
            credential: Credential::Bearer(Secret::new("tok")),
            allow_hosts: HostSet::new(allow.to_vec()),
            rate: 0.0,
            retries: 0,
            max_backoff: Duration::from_secs(1),
            max_artifact_size: 1 << 20,
            spool_dir: dir.to_path_buf(),
            timeout: Duration::from_secs(10),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_source_credential_never_leaves_its_host_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (seen_a, seen_b): (Seen, Seen) = Default::default();
        let a = serve(recording(seen_a.clone(), b"a")).await;
        let b = serve(recording(seen_b.clone(), b"b")).await;
        let other = format!("http://localhost:{b}/x.tgz");

        let g = gate(a, &[], dir.path());
        let err = g.bytes(&Req::get(Url::parse(&other).unwrap()), 10).await.unwrap_err();
        assert!(matches!(err, FetchError::Refused(ref m) if m.contains("--allow-source-host")), "{err:?}");
        assert!(seen_b.lock().unwrap().is_empty(), "a refused host is never contacted");

        let g = gate(a, &[format!("localhost:{b}")], dir.path());
        let got = g.bytes(&Req::get(Url::parse(&other).unwrap()), 10).await.unwrap();
        assert_eq!(&got[..], b"b");
        g.bytes(&Req::get(Url::parse(&format!("http://127.0.0.1:{a}/p")).unwrap()), 10).await.unwrap();
        assert_eq!(seen_b.lock().unwrap().as_slice(), &[("/x.tgz".to_string(), false)]);
        assert_eq!(seen_a.lock().unwrap().as_slice(), &[("/p".to_string(), true)]);
    }

    #[tokio::test]
    async fn an_anonymous_401_is_a_named_refusal_not_a_retry_with_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let hits = Arc::new(Mutex::new(0));
        let h = hits.clone();
        let b = serve(axum::Router::new().fallback(move || {
            let h = h.clone();
            async move {
                *h.lock().unwrap() += 1;
                (StatusCode::UNAUTHORIZED, "no")
            }
        }))
        .await;
        let a = serve(axum::Router::new()).await;
        let g = gate(a, &[format!("localhost:{b}")], dir.path());
        let err = g.bytes(&Req::get(Url::parse(&format!("http://localhost:{b}/x")).unwrap()), 10).await.unwrap_err();
        assert!(matches!(err, FetchError::Refused(ref m) if m.contains("localhost")), "{err:?}");
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn source_supplied_url_pointing_at_loopback_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = serve(axum::Router::new()).await;
        let g = gate(a, &[], dir.path());
        for url in ["http://localhost:1/x", "http://10.0.0.1/x", "file:///etc/passwd"] {
            let err = g.bytes(&Req::get(Url::parse(url).unwrap()), 10).await.unwrap_err();
            assert!(matches!(err, FetchError::Refused(_)), "{url}: {err:?}");
        }
    }

    fn redirecting(to: String) -> axum::Router {
        axum::Router::new().fallback(move || {
            let to = to.clone();
            async move { axum::response::Redirect::temporary(&to) }
        })
    }

    #[tokio::test]
    async fn allowed_source_host_is_followed_through_a_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let seen: Seen = Default::default();
        let b = serve(recording(seen.clone(), b"blob")).await;
        let a = serve(redirecting(format!("http://localhost:{b}/blob?X-Amz-Signature=s"))).await;
        let g = gate(a, &[format!("localhost:{b}")], dir.path());
        let s = g.spool(Url::parse(&format!("http://127.0.0.1:{a}/d")).unwrap(), &Digests::default()).await.unwrap();
        assert_eq!(s.bytes().await.unwrap(), b"blob");
        assert!(s.unverified);
        assert_eq!(seen.lock().unwrap().as_slice(), &[("/blob".to_string(), false)]);
    }

    #[tokio::test]
    async fn redirect_to_a_private_host_without_the_flag_is_a_named_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let seen: Seen = Default::default();
        let b = serve(recording(seen.clone(), b"blob")).await;
        let a = serve(redirecting(format!("http://localhost:{b}/blob"))).await;
        let g = gate(a, &[], dir.path());
        let want = Digests { sha256: Some("00".repeat(32)), ..Default::default() };
        let err = g.spool(Url::parse(&format!("http://127.0.0.1:{a}/d")).unwrap(), &want).await.err().unwrap();
        assert!(matches!(err, FetchError::Refused(ref m) if m.contains("localhost")), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "no 3xx body is ever spooled");
    }

    #[tokio::test]
    async fn spool_verifies_caps_and_unlinks() {
        let dir = tempfile::tempdir().unwrap();
        let a = serve(recording(Default::default(), b"hello")).await;
        let g = gate(a, &[], dir.path());
        let url = Url::parse(&format!("http://127.0.0.1:{a}/f")).unwrap();
        let bad = Digests { sha1: Some("00".repeat(20)), ..Default::default() };
        assert!(matches!(g.spool(url.clone(), &bad).await.err().unwrap(), FetchError::Checksum(_)));
        let good = Digests { sha1: Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".into()), ..Default::default() };
        let s = g.spool(url.clone(), &good).await.unwrap();
        assert!(!s.unverified);
        assert_eq!(s.size, 5);
        let path = s.path.clone();
        drop(s);
        assert!(!path.exists());
        let mut small = gate(a, &[], dir.path());
        small.max_artifact_size = 4;
        assert_eq!(
            small.spool(url, &Digests::default()).await.err().unwrap(),
            FetchError::TooLarge { size: 5, limit: 4 }
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn retry_after_is_honoured_then_the_request_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let hits = Arc::new(Mutex::new(0));
        let h = hits.clone();
        let a = serve(axum::Router::new().fallback(move || {
            let h = h.clone();
            async move {
                let mut n = h.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "slow down").into_response()
                } else {
                    "ok".into_response()
                }
            }
        }))
        .await;
        let mut g = gate(a, &[], dir.path());
        g.retries = 3;
        let got = g.bytes(&Req::get(Url::parse(&format!("http://127.0.0.1:{a}/")).unwrap()), 10).await.unwrap();
        assert_eq!(&got[..], b"ok");
        assert_eq!(*hits.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn permanent_401_does_not_retry() {
        let dir = tempfile::tempdir().unwrap();
        let hits = Arc::new(Mutex::new(0));
        let h = hits.clone();
        let a = serve(axum::Router::new().fallback(move || {
            let h = h.clone();
            async move {
                *h.lock().unwrap() += 1;
                StatusCode::UNAUTHORIZED
            }
        }))
        .await;
        let mut g = gate(a, &[], dir.path());
        g.retries = 3;
        let err = g.bytes(&Req::get(Url::parse(&format!("http://127.0.0.1:{a}/")).unwrap()), 10).await.unwrap_err();
        assert!(matches!(err, FetchError::Auth(_)));
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    use axum::response::IntoResponse as _;

    #[test]
    fn host_set_matches_host_or_host_and_port() {
        let s = HostSet::new(["minio.internal".to_string(), "127.0.0.1:9000".to_string()]);
        assert!(s.contains(&Url::parse("http://minio.internal:1234/x").unwrap()));
        assert!(s.contains(&Url::parse("http://127.0.0.1:9000/x").unwrap()));
        assert!(!s.contains(&Url::parse("http://127.0.0.1:9001/x").unwrap()));
    }
}
