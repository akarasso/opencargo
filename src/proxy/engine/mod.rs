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
use crate::storage::StorageBackend;

use super::auth::{send_with_auth, TokenCache};
use super::singleflight::Singleflight;
use super::strategy::{CacheKey, CachePolicy, Classified, Ttl, UpstreamStrategy, UrlSource};

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
    storage: Arc<dyn StorageBackend>,
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

#[allow(clippy::large_enum_variant)]
enum Lookup {
    Fresh(Cached),
    Negative,
    Stale(Stale),
    Cold,
}

/// Whether an upstream miss is remembered as a negative row or ignored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Miss {
    Record,
    Ignore,
}

impl ProxyEngine {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
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
        self.run(s, up, member, a, false, Miss::Record).await
    }

    /// `fetch` for a recorder: a fresh row is a hit and a cold key is
    /// fetched, but a miss or failure leaves no negative row and no file
    /// deleted, so recording never changes what clients are served.
    pub async fn observe<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Outcome<Cached>> {
        self.run(s, up, member, a, false, Miss::Ignore).await
    }

    /// A conditional re-fetch of a `status 200` row whatever its freshness,
    /// read-only on failure like `observe`: for a packument that may
    /// predate the version a recorder asks about.
    pub async fn refresh<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Outcome<Cached>> {
        self.run(s, up, member, a, true, Miss::Ignore).await
    }

    /// Lookup, singleflight, exchange, settle; cache hit and miss are
    /// counted for client fetches only.
    async fn run<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        force_stale: bool,
        miss: Miss,
    ) -> AppResult<Outcome<Cached>> {
        let key = s.cache_key(a);
        let _guard = self.lock(member, &key, force_stale).await;
        let counted = miss == Miss::Record;
        let stale = match self.lookup(s, member, a, &key, force_stale).await? {
            Lookup::Fresh(cached) => {
                if counted {
                    crate::telemetry::record_cache_hit(&member.0.name);
                }
                return Ok(Outcome::Found(cached));
            }
            Lookup::Negative => {
                if counted {
                    crate::telemetry::record_cache_hit(&member.0.name);
                }
                return Ok(Outcome::NotFound);
            }
            Lookup::Stale(stale) => Some(stale),
            Lookup::Cold => None,
        };
        if counted {
            crate::telemetry::record_cache_miss(&member.0.name);
        }
        let reply = self.exchange(s, up, member, a, stale.as_ref()).await;
        self.settle(s, member, a, reply, stale, miss).await
    }

    /// The cached body, fresh or stale, with no upstream request and no
    /// row touched; `None` when there is none on disk.
    pub async fn peek<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Option<Cached>> {
        let key = s.cache_key(a);
        let Some((row, fresh)) =
            proxy_cache::get_entry(&self.db, member.0.id, key.kind, &key.key).await?
        else {
            return Ok(None);
        };
        if row.status != 200 {
            return Ok(None);
        }
        Ok(self.resolve_row(s, a, row).await.map(|entry| Cached {
            entry,
            stale: !fresh,
        }))
    }

    /// Refreshes coalesce among themselves in their own namespace: a
    /// client's hit on a fresh row never waits behind a recorder's
    /// conditional request.
    async fn lock(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
        refresh: bool,
    ) -> Option<super::singleflight::Guard> {
        let space = if refresh { "refresh/" } else { "" };
        let lock_key = format!("{space}{}/{}/{}", member.0.id, key.kind, key.key);
        self.inflight
            .acquire(&lock_key, self.timeouts.singleflight_wait)
            .await
    }

    /// What the row says before any request; `force_stale` turns a fresh
    /// `200` row into a conditional request instead of a hit.
    async fn lookup<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        key: &CacheKey,
        force_stale: bool,
    ) -> AppResult<Lookup> {
        let Some((row, fresh)) =
            proxy_cache::get_entry(&self.db, member.0.id, key.kind, &key.key).await?
        else {
            return Ok(Lookup::Cold);
        };
        if row.status != 200 {
            return Ok(if fresh {
                Lookup::Negative
            } else {
                Lookup::Cold
            });
        }
        let Some(target) = self.resolve_row(s, a, row.clone()).await else {
            return Ok(Lookup::Cold);
        };
        if fresh && !force_stale {
            return Ok(Lookup::Fresh(self.hit(&row, target).await?));
        }
        Ok(Lookup::Stale(Stale { row, target }))
    }

    async fn settle<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        reply: AppResult<Reply>,
        stale: Option<Stale>,
        miss: Miss,
    ) -> AppResult<Outcome<Cached>> {
        let key = s.cache_key(a);
        match reply {
            Ok(Reply::NotModified) => match stale {
                Some(Stale { row, target }) => {
                    let ttl = self.ttl_secs(s.cache_policy(a));
                    proxy_cache::touch_entry(&self.db, row.id, ttl).await?;
                    // A content-addressed target keeps its own (immutable) policy.
                    if target.id != row.id {
                        proxy_cache::touch_entry(&self.db, target.id, None).await?;
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
            Ok(Reply::Miss(status)) if miss == Miss::Record => {
                self.record_miss(member, &key, status, stale.as_ref())
                    .await?;
                Ok(Outcome::NotFound)
            }
            Ok(Reply::Miss(_)) | Ok(Reply::Refused) => Ok(Outcome::NotFound),
            Ok(Reply::Failed(why)) => match stale {
                Some(Stale { target, .. }) if miss == Miss::Record => {
                    warn!(key = %key.key, error = %why, "upstream failed; serving stale cache");
                    Ok(Outcome::Found(Cached {
                        entry: target,
                        stale: true,
                    }))
                }
                Some(_) => Ok(Outcome::NotFound),
                None => Err(AppError::BadGateway(why)),
            },
            Err(e) if miss == Miss::Ignore => {
                warn!(key = %key.key, error = %e, "refresh failed; cache left as is");
                Ok(Outcome::NotFound)
            }
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
        p.to_response(self.storage.as_ref(), extra).await
    }

    pub async fn purge_repo(&self, member: CacheRepo<'_>) -> AppResult<()> {
        proxy_cache::delete_entries(&self.db, member.0.id).await?;
        proxy_cache::delete_legacy_meta(&self.db, member.0.id).await?;
        self.storage
            .delete_prefix(&format!("_proxy_cache/{}", member.0.name))
            .await?;
        Ok(())
    }

    /// A negative row under `cache_key`, never `store_key`: no body, no sha256.
    /// The body a stale row of its own held loses its last reference here, so
    /// it goes now; a pointer's target stays shared by digest.
    async fn record_miss(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
        status: StatusCode,
        stale: Option<&Stale>,
    ) -> AppResult<()> {
        if let Some(path) = stale.and_then(|st| st.row.storage_path.as_deref()) {
            self.storage.delete(path).await?;
        }
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
        let url = self.guarded_url(s, up, a).await?;
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
            Classified::Miss | Classified::Refused => Ok(Outcome::NotFound),
            Classified::Fail => Err(AppError::BadGateway(format!(
                "upstream HEAD answered {status}"
            ))),
        }
    }

    /// The upstream URL of an artifact, refused when upstream content chose
    /// a host the proxy must not reach.
    pub(super) async fn guarded_url<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        a: &S::Artifact,
    ) -> AppResult<reqwest::Url> {
        let url = s.upstream_url(up, a)?;
        if s.url_source(up, a)
            == (UrlSource::Content {
                allow_private: false,
            })
        {
            super::refuse_blocked_host(&url).await?;
        }
        Ok(url)
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
pub(crate) mod fixture;
#[cfg(test)]
mod tests;
