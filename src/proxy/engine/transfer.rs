use std::future::Future;

use axum::http::{header, HeaderMap, StatusCode};
use bytes::BytesMut;
use sha2::{Digest, Sha256};

use crate::db::proxy_cache::{self, CacheEntry, NewEntry};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{CacheRepo, Upstream};

use super::super::auth::send_with_auth;
use super::super::strategy::{Classified, Transfer, UpstreamStrategy};
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

enum Sink {
    Buffer(BytesMut),
    Part(PartFile),
}

impl Sink {
    async fn push(&mut self, chunk: &[u8]) -> AppResult<()> {
        match self {
            Sink::Buffer(buf) => {
                buf.extend_from_slice(chunk);
                Ok(())
            }
            Sink::Part(part) => part.write_chunk(chunk).await,
        }
    }
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
        let send = send_with_auth(
            &self.http,
            &self.tokens,
            member,
            up,
            req,
            scope.as_deref(),
        );
        match s.transfer(a) {
            Transfer::Buffered => {
                let bounded = tokio::time::timeout(
                    self.timeouts.buffered_total,
                    self.complete(s, member, a, send),
                );
                match bounded.await {
                    Ok(reply) => reply,
                    Err(_) => Ok(Reply::Failed(format!(
                        "upstream {url} exceeded the buffered transfer timeout"
                    ))),
                }
            }
            Transfer::Streamed => self.complete(s, member, a, send).await,
        }
    }

    async fn complete<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        send: impl Future<Output = AppResult<reqwest::Response>>,
    ) -> AppResult<Reply> {
        let resp = match send.await {
            Ok(resp) => resp,
            Err(AppError::BadGateway(why)) => return Ok(Reply::Failed(why)),
            Err(e) => return Err(e),
        };
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
            Ok(body) => self.record(s, member, a, body).await,
            Err(why) => Ok(Reply::Failed(why)),
        }
    }

    /// Hash, cap and verify the body, then land it under `store_key` by rename.
    /// The inner `Err` is a mid-body transport failure.
    async fn read_body<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        mut resp: reqwest::Response,
    ) -> AppResult<Result<Body, String>> {
        let headers = resp.headers().clone();
        let max = s.max_bytes(a);
        let part_rel = format!(
            "{}.part-{}",
            cache_path(member, &s.cache_key(a)),
            uuid::Uuid::new_v4()
        );
        let mut sink = match s.transfer(a) {
            Transfer::Buffered => Sink::Buffer(BytesMut::new()),
            Transfer::Streamed => Sink::Part(PartFile::new(&self.storage, part_rel.clone()).await?),
        };
        let mut hasher = Sha256::new();
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
            hasher.update(&chunk);
            sink.push(&chunk).await?;
        }
        let sha256 = format!("{:x}", hasher.finalize());
        if s.expected_sha256(a)
            .is_some_and(|expected| expected != sha256)
        {
            return Err(AppError::BadGateway("upstream body digest mismatch".into()));
        }
        s.verify_headers(a, &headers, &sha256)?;
        let part = match sink {
            Sink::Buffer(buf) => {
                let mut part = PartFile::new(&self.storage, part_rel).await?;
                part.write_chunk(&buf).await?;
                part
            }
            Sink::Part(part) => part,
        };
        let path = cache_path(member, &s.store_key(a, &sha256));
        part.commit(&self.storage, &path).await?;
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
        proxy_cache::upsert_entry(&self.db, &body_row).await?;
        if pointer {
            let pointer_row = NewEntry {
                kind: key.kind,
                cache_key: &key.key,
                storage_path: None,
                ttl_secs: ttl,
                ..body_row
            };
            proxy_cache::upsert_entry(&self.db, &pointer_row).await?;
        }
        let (entry, _) =
            proxy_cache::get_entry(&self.db, member.0.id, store_key.kind, &store_key.key)
                .await?
                .ok_or_else(|| AppError::Internal("cache row vanished after upsert".into()))?;
        Ok(Reply::Stored(Box::new(entry)))
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}
