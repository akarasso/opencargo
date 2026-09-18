//! The target side: one lane every sink publishes through, and the admin
//! calls a run makes on the target opencargo.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode, Url};
use tokio::io::AsyncReadExt;

use super::http::{retry_after, Secret};
use crate::domain::Format;
use crate::ports::import::{redact, CopyError, TargetAdmin, TargetError, TargetIdentity};

/// 48 KiB: a multiple of 3, so base64 of the chunks concatenates to base64
/// of the whole.
pub const B64_CHUNK: usize = 48 * 1024;

/// A request body the lane can rebuild for every attempt.
#[derive(Clone)]
pub enum Body {
    Empty,
    Bytes(Bytes),
    /// `prefix`, then the file (base64-encoded if asked), then `suffix`:
    /// the npm and cargo publish frames, streamed from the spool.
    Framed { prefix: Bytes, file: PathBuf, file_len: u64, base64: bool, suffix: Bytes },
}

pub fn base64_len(n: u64) -> u64 {
    n.div_ceil(3) * 4
}

impl Body {
    pub fn len(&self) -> u64 {
        match self {
            Body::Empty => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::Framed { prefix, file_len, base64, suffix, .. } => {
                prefix.len() as u64
                    + if *base64 { base64_len(*file_len) } else { *file_len }
                    + suffix.len() as u64
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    async fn build(&self) -> Result<reqwest::Body, CopyError> {
        Ok(match self {
            Body::Empty => reqwest::Body::from(Bytes::new()),
            Body::Bytes(b) => reqwest::Body::from(b.clone()),
            Body::Framed { .. } => reqwest::Body::wrap_stream(self.stream().await?),
        })
    }

    async fn stream(
        &self,
    ) -> Result<futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>, CopyError> {
        Ok(match self {
            Body::Empty => futures_util::stream::empty().boxed(),
            Body::Bytes(b) => futures_util::stream::once(std::future::ready(Ok(b.clone()))).boxed(),
            Body::Framed { prefix, file, base64, suffix, .. } => {
                let f = tokio::fs::File::open(file)
                    .await
                    .map_err(|e| CopyError::Permanent(format!("spool: {e}")))?;
                let encode = *base64;
                let chunks = futures_util::stream::unfold(Some(f), move |state| async move {
                    let mut f = state?;
                    let mut buf = vec![0u8; B64_CHUNK];
                    let mut filled = 0;
                    while filled < B64_CHUNK {
                        match f.read(&mut buf[filled..]).await {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(e) => return Some((Err(e), None)),
                        }
                    }
                    if filled == 0 {
                        return None;
                    }
                    buf.truncate(filled);
                    let out = if encode {
                        Bytes::from(base64::engine::general_purpose::STANDARD.encode(&buf))
                    } else {
                        Bytes::from(buf)
                    };
                    Some((Ok(out), Some(f)))
                });
                let stream = futures_util::stream::once({
                    let p = prefix.clone();
                    async move { Ok::<_, std::io::Error>(p) }
                })
                .chain(chunks)
                .chain(futures_util::stream::once({
                    let s = suffix.clone();
                    async move { Ok::<_, std::io::Error>(s) }
                }));
                stream.boxed()
            }
        })
    }
}

pub struct TReq {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
    /// Counts against the target's per-user publish window.
    pub publish: bool,
}

impl TReq {
    pub fn new(method: Method, url: Url) -> Self {
        Self { method, url, headers: Vec::new(), body: Body::Empty, publish: false }
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }
    pub fn publish(mut self) -> Self {
        self.publish = true;
        self
    }
}

pub struct LaneConfig {
    pub base: Url,
    pub token: Secret,
    /// Publishes per minute, under the target's own per-user limiter.
    pub publish_rate: u32,
    pub cooldown: Duration,
    /// Consecutive cooldowns before the run stops.
    pub stall_after: u32,
    pub timeout: Duration,
}

type Observer = Arc<dyn Fn(&Method, &Url) + Send + Sync>;

/// The target-side chokepoint: the pooled client, the publish window, the
/// cooldown every worker parks on after a 429 or 503.
pub struct TargetLane {
    client: reqwest::Client,
    base: Url,
    token: Secret,
    publish_rate: u32,
    window: tokio::sync::Mutex<VecDeque<Instant>>,
    cooldown: Duration,
    stall_after: u32,
    cooling_until: Mutex<Option<Instant>>,
    episodes: AtomicU32,
    observer: Option<Observer>,
}

impl TargetLane {
    pub fn new(cfg: LaneConfig) -> Result<Self, String> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(cfg.timeout)
            .user_agent(concat!("opencargo-import/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            base: cfg.base,
            token: cfg.token,
            publish_rate: cfg.publish_rate,
            window: tokio::sync::Mutex::new(VecDeque::new()),
            cooldown: cfg.cooldown,
            stall_after: cfg.stall_after,
            cooling_until: Mutex::new(None),
            episodes: AtomicU32::new(0),
            observer: None,
        })
    }

    pub fn observe(mut self, f: impl Fn(&Method, &Url) + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(f));
        self
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn url(&self, path: &str) -> Url {
        self.base.join(path.trim_start_matches('/')).unwrap_or_else(|_| self.base.clone())
    }

    async fn wait_window(&self) {
        if self.publish_rate == 0 {
            return;
        }
        loop {
            let wait = {
                let mut w = self.window.lock().await;
                let now = Instant::now();
                while w.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
                    w.pop_front();
                }
                if (w.len() as u32) < self.publish_rate {
                    w.push_back(now);
                    return;
                }
                Duration::from_secs(60).saturating_sub(now.duration_since(*w.front().unwrap_or(&now)))
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn wait_cooldown(&self) {
        let until = *self.cooling_until.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(until) = until {
            let now = Instant::now();
            if until > now {
                tokio::time::sleep(until - now).await;
            }
        }
    }

    /// Returns `true` when the run should stop.
    fn cool(&self, started: Instant, wait: Duration) -> bool {
        let mut until = self.cooling_until.lock().unwrap_or_else(|p| p.into_inner());
        if until.is_some_and(|u| started < u) {
            return false;
        }
        *until = Some(Instant::now() + wait);
        self.episodes.fetch_add(1, Ordering::SeqCst) + 1 > self.stall_after
    }

    pub async fn send(&self, r: &TReq) -> Result<reqwest::Response, CopyError> {
        loop {
            self.wait_cooldown().await;
            if r.publish {
                self.wait_window().await;
            }
            let started = Instant::now();
            let mut req = self
                .client
                .request(r.method.clone(), r.url.clone())
                .bearer_auth(self.token.expose())
                .body(r.body.build().await?);
            if !matches!(r.body, Body::Empty) {
                req = req.header(reqwest::header::CONTENT_LENGTH, r.body.len());
            }
            for (n, v) in &r.headers {
                req = req.header(*n, v);
            }
            if let Some(obs) = &self.observer {
                obs(&r.method, &r.url);
            }
            let resp = req.send().await.map_err(|e| {
                CopyError::Transient(format!("target {}: {}", redact(&r.url), e.without_url()))
            })?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
                let wait = retry_after(resp.headers()).unwrap_or(self.cooldown).min(self.cooldown.max(Duration::from_secs(1)));
                let text = resp.text().await.unwrap_or_default();
                if self.cool(started, wait) {
                    return Err(CopyError::Stalled(format!(
                        "the target kept answering {status} through {} cooldowns: {}",
                        self.stall_after,
                        text.trim()
                    )));
                }
                tracing::warn!(status = %status, wait_secs = wait.as_secs_f64(), "target lane cooling down");
                continue;
            }
            self.episodes.store(0, Ordering::SeqCst);
            return Ok(resp);
        }
    }
}

/// The target's error text, `{"error": ...}` or raw.
pub async fn error_text(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or(body);
    format!("{status}: {}", msg.trim())
}

/// The target's publish-time vulnerability gate refuses in prose only; this
/// string is the coupling, pinned by `target_osv_refusal_becomes_target_refused`.
pub const OSV_REFUSAL: &str = "publish blocked: critical vulnerabilities";

/// Maps a publish answer onto the copy's vocabulary.
pub async fn publish_outcome(resp: reqwest::Response) -> Result<(), CopyError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = error_text(resp).await;
    Err(match status {
        StatusCode::CONFLICT => CopyError::Conflict(text),
        StatusCode::BAD_REQUEST if text.contains(OSV_REFUSAL) => CopyError::Refused(text),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => CopyError::Refused(text),
        s if s.is_server_error() => CopyError::Transient(text),
        _ => CopyError::Permanent(text),
    })
}

pub async fn body_json(resp: reqwest::Response, max: usize) -> Result<serde_json::Value, CopyError> {
    let mut out = Vec::new();
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        let chunk = chunk.map_err(|e| CopyError::Transient(e.without_url().to_string()))?;
        if out.len() + chunk.len() > max {
            return Err(CopyError::Permanent("target answer too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&out).map_err(|e| CopyError::Permanent(format!("target answered non-JSON: {e}")))
}

/// The admin surface over the target's REST API; the admin token is used
/// only for creating repositories, users and grants.
pub struct HttpTargetAdmin {
    lane: Arc<TargetLane>,
    admin: Option<Arc<TargetLane>>,
}

impl HttpTargetAdmin {
    pub fn new(lane: Arc<TargetLane>, admin: Option<Arc<TargetLane>>) -> Self {
        Self { lane, admin }
    }

    fn admin(&self) -> Result<&TargetLane, TargetError> {
        self.admin.as_deref().ok_or_else(|| {
            TargetError::Refused("this needs an admin token in OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN".into())
        })
    }
}

fn target_err(e: CopyError) -> TargetError {
    TargetError::Unavailable(e.to_string())
}

#[async_trait]
impl TargetAdmin for HttpTargetAdmin {
    async fn identity(&self) -> Result<TargetIdentity, TargetError> {
        let url = self.lane.url("api/v1/me/permissions");
        let resp = self.lane.send(&TReq::new(Method::GET, url)).await.map_err(target_err)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(TargetError::Unsupported("/api/v1/me/permissions".into()));
        }
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(TargetError::Refused(format!(
                "the target token is not valid: the target answered {}",
                error_text(resp).await
            )));
        }
        if !resp.status().is_success() {
            return Err(TargetError::Refused(error_text(resp).await));
        }
        let v = body_json(resp, 16 << 20).await.map_err(target_err)?;
        serde_json::from_value(v).map_err(|e| TargetError::Unavailable(format!("unexpected identity: {e}")))
    }

    async fn create_repository(&self, name: &str, format: Format) -> Result<(), TargetError> {
        let lane = self.admin()?;
        let body = serde_json::json!({ "name": name, "type": "hosted", "format": format.as_str(), "visibility": "private" });
        let req = TReq::new(Method::POST, lane.url("api/v1/repositories"))
            .header("content-type", "application/json")
            .body(Body::Bytes(Bytes::from(body.to_string())));
        let resp = lane.send(&req).await.map_err(target_err)?;
        if resp.status().is_success() || resp.status() == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(TargetError::Refused(format!("creating {name}: {}", error_text(resp).await)))
        }
    }

    async fn user_exists(&self, username: &str) -> Result<bool, TargetError> {
        let lane = self.admin()?;
        let resp = lane
            .send(&TReq::new(Method::GET, lane.url(&format!("api/v1/users/{username}"))))
            .await
            .map_err(target_err)?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(TargetError::Refused(error_text(resp).await)),
        }
    }

    async fn grant(&self, username: &str, repo: &str, read: bool, write: bool) -> Result<(), TargetError> {
        let lane = self.admin()?;
        let body = serde_json::json!({ "can_read": read, "can_write": write });
        let req = TReq::new(Method::PUT, lane.url(&format!("api/v1/users/{username}/permissions/{repo}")))
            .header("content-type", "application/json")
            .body(Body::Bytes(Bytes::from(body.to_string())));
        let resp = lane.send(&req).await.map_err(target_err)?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(TargetError::Refused(format!("granting {username} on {repo}: {}", error_text(resp).await)))
        }
    }

    async fn create_user(&self, username: &str) -> Result<(), TargetError> {
        let lane = self.admin()?;
        let body = serde_json::json!({ "username": username, "role": "reader" });
        let req = TReq::new(Method::POST, lane.url("api/v1/users"))
            .header("content-type", "application/json")
            .body(Body::Bytes(Bytes::from(body.to_string())));
        let resp = lane.send(&req).await.map_err(target_err)?;
        let status = resp.status();
        // The 201 carries a generated password: read and dropped here, so it
        // reaches no report, no stdout and no log.
        drop(resp.bytes().await);
        if status.is_success() || status == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(TargetError::Refused(format!("creating user {username}: {status}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn base64_chunking_matches_whole_file_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let data: Vec<u8> = (0..(B64_CHUNK * 2 + 7)).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let body = Body::Framed {
            prefix: Bytes::from_static(b"{\"d\":\""),
            file: path,
            file_len: data.len() as u64,
            base64: true,
            suffix: Bytes::from_static(b"\"}"),
        };
        let mut s = body.stream().await.unwrap();
        let mut out = Vec::new();
        while let Some(c) = s.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        let expected = format!("{{\"d\":\"{}\"}}", base64::engine::general_purpose::STANDARD.encode(&data));
        assert_eq!(String::from_utf8(out).unwrap(), expected);
        assert_eq!(body.len(), expected.len() as u64);
    }
}
