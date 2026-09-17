mod payload;
mod transfer;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::proxy_cache::{self, CacheEntry, NewEntry};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{CacheRepo, Outcome, Upstream};
use crate::storage::{FilesystemStorage, StorageBackend};

use super::auth::{send_with_auth, TokenCache};
use super::singleflight::Singleflight;
use super::strategy::{CacheKey, CachePolicy, Classified, Ttl, UpstreamStrategy};

pub use payload::{cache_path, Cached, PartFile, Payload, Src};
use transfer::Reply;

#[derive(Clone, Copy, Debug)]
pub struct TtlConfig {
    pub default_secs: u64,
    pub negative_secs: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    pub read_idle: Duration,
    pub buffered_total: Duration,
    pub singleflight_wait: Duration,
}

impl Timeouts {
    pub fn from_connect_secs(n: u64) -> Self {
        Self {
            connect: Duration::from_secs(n),
            read_idle: Duration::from_secs(3 * n),
            buffered_total: Duration::from_secs(6 * n),
            singleflight_wait: Duration::from_secs(6 * n),
        }
    }
}

#[derive(Clone)]
pub struct ProxyEngine {
    http: reqwest::Client,
    storage: Arc<FilesystemStorage>,
    db: SqlitePool,
    tokens: Arc<TokenCache>,
    inflight: Arc<Singleflight>,
    ttl: TtlConfig,
    timeouts: Timeouts,
}

/// A stale `status 200` row and its resolved on-disk target.
struct Stale {
    row: CacheEntry,
    target: CacheEntry,
}

impl ProxyEngine {
    pub fn new(
        storage: Arc<FilesystemStorage>,
        db: SqlitePool,
        timeouts: Timeouts,
        ttl: TtlConfig,
    ) -> Self {
        // Per-chunk idleness, never a client-wide total: a multi-minute blob must live.
        let http = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read_idle)
            .redirect(super::redirect_policy())
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            storage,
            db,
            tokens: Arc::new(TokenCache::default()),
            inflight: Arc::new(Singleflight::default()),
            ttl,
            timeouts,
        }
    }

    pub async fn fetch<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Outcome<Cached>> {
        let key = s.cache_key(a);
        let lock_key = format!("{}/{}/{}", member.0.id, key.kind, key.key);
        let _guard = self
            .inflight
            .acquire(&lock_key, self.timeouts.singleflight_wait)
            .await;
        let mut stale = None;
        if let Some((row, fresh)) =
            proxy_cache::get_entry(&self.db, member.0.id, key.kind, &key.key).await?
        {
            if row.status != 200 {
                if fresh {
                    return Ok(Outcome::NotFound);
                }
            } else if let Some(target) = self.resolve_row(s, a, row.clone()).await {
                if fresh {
                    return Ok(Outcome::Found(self.hit(&row, target).await?));
                }
                stale = Some(Stale { row, target });
            }
        }
        let reply = self.exchange(s, up, member, a, stale.as_ref()).await;
        match reply {
            Ok(Reply::NotModified) => match stale {
                Some(Stale { row, target }) => {
                    let ttl = self.ttl_secs(s.cache_policy(a));
                    proxy_cache::touch_entry(&self.db, row.id, ttl).await?;
                    if target.id != row.id {
                        proxy_cache::touch_entry(&self.db, target.id, ttl).await?;
                    }
                    Ok(Outcome::Found(Cached {
                        entry: target,
                        stale: false,
                    }))
                }
                None => Err(AppError::BadGateway(
                    "upstream answered 304 to an unconditional request".into(),
                )),
            },
            Ok(Reply::Stored(entry)) => Ok(Outcome::Found(Cached {
                entry: *entry,
                stale: false,
            })),
            Ok(Reply::Miss(status)) => {
                self.record_miss(member, &key, status).await?;
                Ok(Outcome::NotFound)
            }
            Ok(Reply::Failed(why)) => match stale {
                Some(Stale { target, .. }) => {
                    warn!(key = %lock_key, error = %why, "upstream failed; serving stale cache");
                    Ok(Outcome::Found(Cached {
                        entry: target,
                        stale: true,
                    }))
                }
                None => Err(AppError::BadGateway(why)),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn head<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Outcome<Payload>> {
        let key = s.cache_key(a);
        let row = proxy_cache::get_entry(&self.db, member.0.id, key.kind, &key.key).await?;
        if let Some((row, true)) = row {
            if row.status != 200 {
                return Ok(Outcome::NotFound);
            }
            if let Some(target) = self.resolve_row(s, a, row.clone()).await {
                let e = self.hit(&row, target).await?.entry;
                let size = e.size.max(0) as u64;
                return Ok(Outcome::Found(Payload::head_only(
                    size,
                    e.content_type,
                    e.digest,
                )));
            }
        }
        if s.head_via_get(a) {
            return Ok(self.fetch(s, up, member, a).await?.into_payload());
        }
        self.forward_head(s, up, member, a).await
    }

    pub async fn bytes(&self, c: &Cached) -> AppResult<Bytes> {
        let path = c
            .entry
            .storage_path
            .as_deref()
            .ok_or_else(|| AppError::BadGateway("cache row has no file".into()))?;
        self.storage
            .get(path)
            .await
            .map_err(|e| AppError::BadGateway(format!("cached file unreadable: {e}")))
    }

    pub async fn stream_response(
        &self,
        p: &Payload,
        extra: Vec<(HeaderName, HeaderValue)>,
    ) -> AppResult<Response> {
        p.to_response(&self.storage, extra).await
    }

    pub async fn purge_repo(&self, member: CacheRepo<'_>) -> AppResult<()> {
        proxy_cache::delete_entries(&self.db, member.0.id).await?;
        proxy_cache::delete_legacy_meta(&self.db, member.0.id).await?;
        self.storage
            .delete_prefix(&format!("_proxy_cache/{}", member.0.name))
            .await
    }

    /// A negative row under `cache_key`, never `store_key`: no body, no sha256.
    async fn record_miss(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
        status: StatusCode,
    ) -> AppResult<()> {
        let entry = NewEntry {
            repository_id: member.0.id,
            kind: key.kind,
            cache_key: &key.key,
            status: i64::from(status.as_u16()),
            storage_path: None,
            content_type: None,
            etag: None,
            digest: None,
            size: 0,
            ttl_secs: Some(self.ttl.negative_secs),
        };
        proxy_cache::upsert_entry(&self.db, &entry).await?;
        Ok(())
    }

    /// The row whose file exists: a pointer's target, or the row itself.
    async fn resolve_row<S: UpstreamStrategy>(
        &self,
        s: &S,
        a: &S::Artifact,
        row: CacheEntry,
    ) -> Option<CacheEntry> {
        let target = match (&row.storage_path, &row.digest) {
            (Some(_), _) => row,
            (None, Some(digest)) => {
                let key = s.store_key(a, digest);
                proxy_cache::get_entry(&self.db, row.repository_id, key.kind, &key.key)
                    .await
                    .ok()
                    .flatten()
                    .map(|(target, _)| target)?
            }
            (None, None) => return None,
        };
        let path = target.storage_path.as_deref()?;
        self.storage.exists(path).await.ok()?.then_some(target)
    }

    // An untouched pointer would be evicted under a hot tag.
    async fn hit(&self, row: &CacheEntry, target: CacheEntry) -> AppResult<Cached> {
        proxy_cache::touch_entry(&self.db, row.id, None).await?;
        if target.id != row.id {
            proxy_cache::touch_entry(&self.db, target.id, None).await?;
        }
        Ok(Cached {
            entry: target,
            stale: false,
        })
    }

    /// One upstream HEAD, no lock, no row: for artifacts too large to warm on HEAD.
    async fn forward_head<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Outcome<Payload>> {
        let url = s.upstream_url(up, a)?;
        let mut req = self.http.head(url);
        for (name, value) in s.request_headers(a) {
            req = req.header(name, value);
        }
        let resp = send_with_auth(
            &self.http,
            &self.tokens,
            member,
            up,
            req,
            s.bearer_scope(a).as_deref(),
        )
        .await?;
        let status = resp.status();
        if status.is_success() {
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            // Not `resp.content_length()`: that is the body size hint, 0 for a HEAD.
            let size = resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return Ok(Outcome::Found(Payload::head_only(
                size,
                content_type,
                s.expected_sha256(a),
            )));
        }
        match s.classify_status(a, status) {
            Classified::Miss => Ok(Outcome::NotFound),
            Classified::Fail => Err(AppError::BadGateway(format!(
                "upstream HEAD answered {status}"
            ))),
        }
    }

    fn ttl_secs(&self, policy: CachePolicy) -> Option<u64> {
        match policy {
            CachePolicy::Immutable => None,
            CachePolicy::Ttl(Ttl::Default) => Some(self.ttl.default_secs),
            CachePolicy::Ttl(Ttl::Secs(n)) => Some(n),
        }
    }
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
