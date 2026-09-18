use std::future::Future;

use axum::http::{header, HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::domain::{CacheEntry, CacheRepo, NewEntry};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::Upstream;

use super::super::auth::send_with_auth;
use super::super::strategy::{
    Classified, DigestAlgorithm, ExpectedDigests, RedirectRule, Transfer, UpstreamStrategy,
};
use super::{cache_path, PartFile, ProxyEngine, Stale};

/// What one upstream exchange ended in; `Failed` is stale-eligible, an
/// integrity refusal (cap, digest, headers) is an `Err` and never serves stale.
pub(super) enum Reply {
    NotModified,
    Stored(Box<CacheEntry>),
    Miss(StatusCode),
    Refused,
    Failed(String),
}

struct Body {
    path: String,
    sha256: String,
    size: u64,
    headers: HeaderMap,
}

impl ProxyEngine {
    pub(super) async fn exchange<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        stale: Option<&Stale>,
        now: DateTime<Utc>,
    ) -> AppResult<Reply> {
        let url = self.guarded_url(s, up, a).await?;
        let mut req = self.http.get(url.clone());
        for (name, value) in s.request_headers(a) {
            req = req.header(name, value);
        }
        if let Some(etag) = stale.and_then(|st| st.row.etag.as_deref()) {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        let scope = s.bearer_scope(a);
        let credentials = s.send_credentials(a);
        let send = async {
            if credentials {
                send_with_auth(&self.http, &self.tokens, member, up, req, scope.as_deref()).await
            } else {
                req.send()
                    .await
                    .map_err(|e| AppError::BadGateway(format!("upstream request failed: {e}")))
            }
        };
        match s.transfer(a) {
            Transfer::Buffered => {
                let bounded = tokio::time::timeout(
                    self.timeouts.buffered_total,
                    self.complete(s, member, a, &url, send, now),
                );
                match bounded.await {
                    Ok(reply) => reply,
                    Err(_) => Ok(Reply::Failed(format!(
                        "upstream {url} exceeded the buffered transfer timeout"
                    ))),
                }
            }
            Transfer::Streamed => self.complete(s, member, a, &url, send, now).await,
        }
    }

    async fn complete<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        asked: &reqwest::Url,
        send: impl Future<Output = AppResult<reqwest::Response>>,
        now: DateTime<Utc>,
    ) -> AppResult<Reply> {
        let resp = match send.await {
            Ok(resp) => resp,
            Err(AppError::BadGateway(why)) => return Ok(Reply::Failed(why)),
            Err(e) => return Err(e),
        };
        if !redirect_allowed(s.final_url_must_match(a), asked, resp.url()) {
            return Err(AppError::BadGateway("upstream redirected off its origin".into()));
        }
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(Reply::NotModified);
        }
        if !status.is_success() {
            return Ok(match s.classify_status(a, status) {
                Classified::Miss => Reply::Miss(status),
                Classified::Refused => Reply::Refused,
                Classified::Fail => Reply::Failed(format!("upstream answered {status}")),
            });
        }
        let body = self.read_body(s, member, a, resp).await?;
        match body {
            Ok(body) => self.record(s, member, a, body, now).await,
            Err(why) => Ok(Reply::Failed(why)),
        }
    }

    /// Hash, cap and verify the body chunk by chunk into a part file, then
    /// land it under `store_key` by rename: no transfer holds its body in
    /// memory, `Transfer` only decides the timeout. The inner `Err` is a
    /// mid-body transport failure.
    async fn read_body<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        mut resp: reqwest::Response,
    ) -> AppResult<Result<Body, String>> {
        let headers = resp.headers().clone();
        let expected = s.expected_digests(a, &headers);
        let mut digests = Hashers::for_expected(&expected);
        let max = s.max_bytes(a);
        let part_rel = format!(
            "{}.part-{}",
            cache_path(member, &s.cache_key(a)),
            uuid::Uuid::new_v4()
        );
        let mut part = PartFile::new(self.storage.as_ref(), part_rel).await?;
        let mut size = 0u64;
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => return Ok(Err(format!("upstream body read failed: {e}"))),
            };
            size += chunk.len() as u64;
            if size > max {
                return Err(AppError::BadGateway(format!(
                    "upstream body exceeds {max} bytes"
                )));
            }
            digests.update(&chunk);
            part.write_chunk(&chunk).await?;
        }
        let computed = digests.finish();
        if let Some(wrong) = expected.mismatch(|alg| computed.get(alg)) {
            return Err(AppError::BadGateway(format!(
                "upstream body digest mismatch ({:?} {:?})",
                wrong.source, wrong.algorithm
            )));
        }
        let sha256 = computed.sha256.clone();
        let path = cache_path(member, &s.store_key(a, &sha256));
        part.commit(self.storage.as_ref(), &path).await?;
        Ok(Ok(Body {
            path,
            sha256,
            size,
            headers,
        }))
    }

    async fn record<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        body: Body,
        now: DateTime<Utc>,
    ) -> AppResult<Reply> {
        let key = s.cache_key(a);
        let store_key = s.store_key(a, &body.sha256);
        let ttl = self.ttl_secs(s.cache_policy(a));
        let content_type = header_str(&body.headers, header::CONTENT_TYPE);
        let etag = header_str(&body.headers, header::ETAG);
        let pointer = store_key != key;
        let body_row = NewEntry {
            repository_id: member.0.id,
            kind: store_key.kind,
            cache_key: &store_key.key,
            status: 200,
            storage_path: Some(&body.path),
            content_type,
            etag,
            digest: Some(&body.sha256),
            size: body.size as i64,
            ttl_secs: if pointer { None } else { ttl },
        };
        self.cache.upsert(&body_row, now).await?;
        if pointer {
            let pointer_row = NewEntry {
                kind: key.kind,
                cache_key: &key.key,
                storage_path: None,
                ttl_secs: ttl,
                ..body_row
            };
            self.cache.upsert(&pointer_row, now).await?;
        }
        let entry = self
            .cache
            .entry(member.0.id, store_key.kind, &store_key.key, now)
            .await?
            .ok_or_else(|| AppError::Internal("cache row vanished after upsert".into()))?;
        Ok(Reply::Stored(Box::new(entry)))
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// sha256 always, for the store key; the others only when an expected digest
/// names them.
struct Hashers {
    sha256: Sha256,
    sha512: Option<Sha512>,
    sha1: Option<Sha1>,
}

struct Computed {
    sha256: String,
    sha512: Option<String>,
    sha1: Option<String>,
}

impl Hashers {
    fn for_expected(expected: &ExpectedDigests) -> Self {
        let wants = |alg| expected.entries().iter().any(|d| d.algorithm == alg);
        Self {
            sha256: Sha256::new(),
            sha512: wants(DigestAlgorithm::Sha512).then(Sha512::new),
            sha1: wants(DigestAlgorithm::Sha1).then(Sha1::new),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        self.sha256.update(chunk);
        if let Some(h) = &mut self.sha512 {
            h.update(chunk);
        }
        if let Some(h) = &mut self.sha1 {
            h.update(chunk);
        }
    }

    fn finish(self) -> Computed {
        Computed {
            sha256: format!("{:x}", self.sha256.finalize()),
            sha512: self.sha512.map(|h| format!("{:x}", h.finalize())),
            sha1: self.sha1.map(|h| format!("{:x}", h.finalize())),
        }
    }
}

impl Computed {
    fn get(&self, alg: DigestAlgorithm) -> Option<String> {
        match alg {
            DigestAlgorithm::Sha256 => Some(self.sha256.clone()),
            DigestAlgorithm::Sha512 => self.sha512.clone(),
            DigestAlgorithm::Sha1 => self.sha1.clone(),
        }
    }
}

fn redirect_allowed(rule: RedirectRule, asked: &reqwest::Url, landed: &reqwest::Url) -> bool {
    match rule {
        RedirectRule::Unrestricted => true,
        RedirectRule::SameOrigin => asked.origin() == landed.origin(),
    }
}
