mod payload;
mod transfer;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::domain::{CacheEntry, CacheRepo, NewEntry, Outcome};
use crate::error::{AppError, AppResult};
use crate::domain::layout;
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::ports::reclaim::ReclaimStore;
use crate::ports::repositories::RepositoryStore;
use crate::registry::resolve::Upstream;
use crate::storage::StorageBackend;

use super::auth::{send_with_auth, TokenCache};
use super::singleflight::Singleflight;
use super::strategy::{
    CacheKey, CachePolicy, Classified, DigestAlgorithm, DigestSource, Ttl, UpstreamStrategy, UrlSource,
};

pub use payload::{cache_path, Cached, IntoPayload, Payload, Src, CACHE_SEGMENT};
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
    cache: Arc<dyn ProxyCacheStore>,
    repos: Arc<dyn RepositoryStore>,
    reclaim: Arc<dyn ReclaimStore>,
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

/// One pass through the engine: the instant every row it writes or reads is
/// judged against, and what a miss means for the caller. Carried together so
/// a single request never mixes two clocks.
#[derive(Clone, Copy)]
struct Pass {
    now: DateTime<Utc>,
    miss: Miss,
    /// Revalidate a fresh `200` row instead of serving it.
    force_stale: bool,
    /// Bounds the upstream awaits and the wait for the singleflight guard,
    /// never a storage write.
    deadline: Option<tokio::time::Instant>,
}

impl Pass {
    fn new(miss: Miss, force_stale: bool) -> Self {
        Self {
            now: Utc::now(),
            miss,
            force_stale,
            deadline: None,
        }
    }
}

impl ProxyEngine {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        cache: Arc<dyn ProxyCacheStore>,
        repos: Arc<dyn RepositoryStore>,
        reclaim: Arc<dyn ReclaimStore>,
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
            cache,
            repos,
            reclaim,
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
        self.run(s, up, member, a, Pass::new(Miss::Record, false))
            .await
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
        self.run(s, up, member, a, Pass::new(Miss::Ignore, false))
            .await
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
        self.run(s, up, member, a, Pass::new(Miss::Ignore, true))
            .await
    }

    /// `observe` whose upstream awaits end at `limit`; `None` when they did.
    /// The storage write of a body that arrived in time is never cut short.
    pub async fn observe_within<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        limit: Duration,
    ) -> AppResult<Option<Outcome<Cached>>> {
        let pass = Pass {
            deadline: Some(tokio::time::Instant::now() + limit),
            ..Pass::new(Miss::Ignore, false)
        };
        self.run_timed(s, up, member, a, pass).await
    }

    async fn run<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        pass: Pass,
    ) -> AppResult<Outcome<Cached>> {
        let settled = self.run_timed(s, up, member, a, pass).await?;
        settled.ok_or_else(|| AppError::BadGateway("upstream deadline exceeded".into()))
    }

    /// Lookup, then singleflight, exchange, settle. A warm answer never
    /// takes the guard; cache hit and miss are counted for client fetches
    /// only.
    async fn run_timed<S: UpstreamStrategy>(
        &self,
        s: &S,
        up: &Upstream,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        pass: Pass,
    ) -> AppResult<Option<Outcome<Cached>>> {
        let key = s.cache_key(a);
        let counted = pass.miss == Miss::Record;
        if !pass.force_stale {
            if let Some(warm) = self.warm(s, member, a, &key, pass, counted).await? {
                return Ok(Some(warm));
            }
        }
        let locking = self.lock(member, &key, pass.force_stale);
        let _guard = match pass.deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, locking).await {
                Ok(guard) => guard,
                Err(_) => return Ok(None),
            },
            None => locking.await,
        };
        let stale = match self.lookup(s, member, a, &key, pass).await? {
            Lookup::Fresh(cached) => {
                if counted {
                    crate::telemetry::record_cache_hit(&member.0.name);
                }
                return Ok(Some(Outcome::Found(cached)));
            }
            Lookup::Negative => {
                if counted {
                    crate::telemetry::record_cache_hit(&member.0.name);
                }
                return Ok(Some(Outcome::NotFound));
            }
            Lookup::Stale(stale) => Some(stale),
            Lookup::Cold => None,
        };
        if counted {
            crate::telemetry::record_cache_miss(&member.0.name);
        }
        if let Some(announced) = announced(s, a) {
            if self
                .cache
                .quarantined(member.0.id, key.kind, &key.key, &announced, pass.now)
                .await?
            {
                return Err(AppError::BadGateway(
                    "upstream body is quarantined under its announced digest".into(),
                ));
            }
        }
        let reply = self
            .exchange(s, up, member, a, stale.as_ref(), pass)
            .await;
        let reply = match reply {
            Ok(Reply::TimedOut(_)) if pass.deadline.is_some() => return Ok(None),
            Ok(Reply::TimedOut(why)) => Ok(Reply::Failed(why)),
            other => other,
        };
        self.settle(s, member, a, reply, stale, pass).await.map(Some)
    }

    /// The fresh answers, read without the singleflight guard.
    async fn warm<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
        key: &CacheKey,
        pass: Pass,
        counted: bool,
    ) -> AppResult<Option<Outcome<Cached>>> {
        let found = match self.lookup(s, member, a, key, pass).await? {
            Lookup::Fresh(cached) => Outcome::Found(cached),
            Lookup::Negative => Outcome::NotFound,
            Lookup::Stale(_) | Lookup::Cold => return Ok(None),
        };
        if counted {
            crate::telemetry::record_cache_hit(&member.0.name);
        }
        Ok(Some(found))
    }

    /// The cached body, fresh or stale, with no upstream request and no
    /// row touched; `None` when there is none on disk.
    pub async fn peek<S: UpstreamStrategy>(
        &self,
        s: &S,
        member: CacheRepo<'_>,
        a: &S::Artifact,
    ) -> AppResult<Option<Cached>> {
        let now = Utc::now();
        let key = s.cache_key(a);
        let Some(row) = self.row(member, &key, now).await? else {
            return Ok(None);
        };
        if row.status != 200 {
            return Ok(None);
        }
        let fresh = row.fresh;
        Ok(self.resolve_row(s, a, row, now).await?.map(|entry| Cached {
            entry,
            stale: !fresh,
        }))
    }

    async fn row(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
        now: DateTime<Utc>,
    ) -> AppResult<Option<CacheEntry>> {
        Ok(self
            .cache
            .entry(member.0.id, key.kind, &key.key, now)
            .await?)
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
        pass: Pass,
    ) -> AppResult<Lookup> {
        let Some(row) = self.row(member, key, pass.now).await? else {
            return Ok(Lookup::Cold);
        };
        if row.status != 200 {
            return Ok(if row.fresh {
                Lookup::Negative
            } else {
                Lookup::Cold
            });
        }
        let Some(target) = self.resolve_row(s, a, row.clone(), pass.now).await? else {
            return Ok(Lookup::Cold);
        };
        if row.fresh && !pass.force_stale {
            return Ok(Lookup::Fresh(self.hit(&row, target, pass.now).await?));
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
        pass: Pass,
    ) -> AppResult<Outcome<Cached>> {
        let key = s.cache_key(a);
        match reply {
            Ok(Reply::NotModified) => match stale {
                Some(Stale { row, target }) => {
                    let ttl = self.ttl(s.cache_policy(a));
                    self.cache.touch(row.id, ttl, pass.now).await?;
                    // A content-addressed target keeps its own (immutable) policy.
                    if target.id != row.id {
                        self.cache.touch(target.id, None, pass.now).await?;
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
            Ok(Reply::Miss(status)) if pass.miss == Miss::Record => {
                self.record_miss(member, &key, status, stale.as_ref(), pass.now)
                    .await?;
                Ok(Outcome::NotFound)
            }
            Ok(Reply::Miss(_)) | Ok(Reply::Refused) => Ok(Outcome::NotFound),
            Ok(Reply::Failed(why) | Reply::TimedOut(why)) => match stale {
                Some(Stale { target, .. }) if pass.miss == Miss::Record => {
                    warn!(key = %key.key, error = %why, "upstream failed; serving stale cache");
                    Ok(Outcome::Found(Cached {
                        entry: target,
                        stale: true,
                    }))
                }
                Some(_) => Ok(Outcome::NotFound),
                None => Err(AppError::BadGateway(why)),
            },
            Ok(Reply::Rejected(why)) => {
                if let Some(announced) = announced(s, a) {
                    self.cache
                        .quarantine(member.0.id, key.kind, &key.key, &announced, &why, pass.now)
                        .await?;
                }
                if pass.miss == Miss::Ignore {
                    warn!(key = %key.key, error = %why, "refresh rejected; cache left as is");
                    return Ok(Outcome::NotFound);
                }
                Err(AppError::BadGateway(why))
            }
            Err(e) if pass.miss == Miss::Ignore => {
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
        let now = Utc::now();
        let key = s.cache_key(a);
        let row = self.row(member, &key, now).await?;
        if let Some(row) = row.filter(|row| row.fresh) {
            if row.status != 200 {
                return Ok(Outcome::NotFound);
            }
            if let Some(target) = self.resolve_row(s, a, row.clone(), now).await? {
                let e = self.hit(&row, target, now).await?.entry;
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

    /// A body this server derived from a cached one, under a key the caller
    /// owns: `None` while it has not been derived, or its file is gone. The
    /// key names what the body was derived from, so a rendering is never
    /// stale — it is replaced by the one under the next source's key.
    pub async fn derived(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
    ) -> AppResult<Option<Payload>> {
        let now = Utc::now();
        let Some(row) = self.row(member, key, now).await? else {
            return Ok(None);
        };
        let Some(path) = row.storage_path.as_deref() else {
            return Ok(None);
        };
        if self.storage.head(path).await?.is_none() {
            return Ok(None);
        }
        self.cache.touch(row.id, None, now).await?;
        Ok(Some(Payload {
            src: Src::File(path.to_string()),
            size: row.size.max(0) as u64,
            content_type: row.content_type,
            digest: None,
            stale: false,
        }))
    }

    /// Remember a derived body, and answer with the file it now lives in:
    /// what a client is served next is streamed, never held. It expires
    /// like an immutable body — the sweep reclaims it once it falls idle.
    pub async fn put_derived(
        &self,
        member: CacheRepo<'_>,
        key: &CacheKey,
        body: Bytes,
        content_type: &str,
    ) -> AppResult<Payload> {
        let now = Utc::now();
        let root = self.cache_root(member).await?;
        let path = cache_path(&root, key);
        let size = body.len() as u64;
        self.storage.put(&path, body).await?;
        let entry = NewEntry {
            repository_id: member.0.id,
            kind: key.kind,
            cache_key: &key.key,
            status: 200,
            storage_path: Some(&path),
            content_type: Some(content_type),
            etag: None,
            digest: None,
            size: size as i64,
            ttl_secs: None,
        };
        if let Err(e) = self.cache.upsert(&entry, now).await {
            self.release(std::slice::from_ref(&path), now).await;
            return Err(e.into());
        }
        Ok(Payload {
            src: Src::File(path),
            size,
            content_type: Some(content_type.to_string()),
            digest: None,
            stale: false,
        })
    }

    pub async fn bytes(&self, c: &Cached) -> AppResult<Bytes> {
        let path = c
            .entry
            .storage_path
            .as_deref()
            .ok_or_else(|| AppError::BadGateway("cache row has no file".into()))?;
        Ok(self.storage.get(path).await?)
    }

    /// The cached body as a stream, for a caller that digests what it serves.
    pub async fn read_stream(&self, c: &Cached) -> AppResult<crate::storage::ReadStream> {
        let path = c
            .entry
            .storage_path
            .as_deref()
            .ok_or_else(|| AppError::BadGateway("cache row has no file".into()))?;
        Ok(self.storage.read_stream(path).await?)
    }

    pub async fn stream_response(
        &self,
        p: &Payload,
        extra: Vec<(HeaderName, HeaderValue)>,
    ) -> AppResult<Response> {
        p.to_response(self.storage.as_ref(), extra).await
    }

    /// Where a member's cached bodies live: under its incarnation, never
    /// its name, so a recreated repository shares no key with the old one.
    pub(super) async fn cache_root(&self, member: CacheRepo<'_>) -> AppResult<String> {
        let incarnation = self
            .repos
            .incarnation(member.0.id)
            .await?
            .ok_or_else(|| AppError::NotFound("repository was removed".into()))?;
        Ok(layout::incarnation_prefix(&incarnation))
    }

    /// Cache keys take no pin and are never deleted inline: every release
    /// is enqueued, and a row left without bytes is healed by a refetch.
    pub(super) async fn release(&self, keys: &[String], now: DateTime<Utc>) {
        if keys.is_empty() {
            return;
        }
        if let Err(e) = self.reclaim.enqueue(keys, now).await {
            warn!(error = %e, ?keys, "cache files not enqueued; the scan will find them");
        }
    }

    /// The rows go, and every file under the member's cache prefixes, its
    /// incarnation's and its legacy name-keyed one, is enqueued.
    pub async fn purge_repo(&self, member: CacheRepo<'_>) -> AppResult<()> {
        use futures_util::TryStreamExt;
        self.cache.delete_for_repo(member.0.id).await?;
        let mut prefixes = vec![format!("_proxy_cache/{}", member.0.name)];
        if let Ok(root) = self.cache_root(member).await {
            prefixes.push(format!("{root}/{CACHE_SEGMENT}"));
        }
        let now = Utc::now();
        for prefix in prefixes {
            let keys: Vec<String> = self
                .storage
                .list(&prefix)
                .map_ok(|meta| meta.key)
                .try_collect()
                .await?;
            self.reclaim.enqueue(&keys, now).await?;
        }
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
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        if let Some(path) = stale.and_then(|st| st.row.storage_path.as_deref()) {
            self.release(&[path.to_string()], now).await;
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
        self.cache.upsert(&entry, now).await?;
        Ok(())
    }

    /// The row whose file exists: a pointer's target, or the row itself.
    /// A store or storage fault is an error, never a miss: a miss would
    /// refetch and overwrite on a hiccup.
    async fn resolve_row<S: UpstreamStrategy>(
        &self,
        s: &S,
        a: &S::Artifact,
        row: CacheEntry,
        now: DateTime<Utc>,
    ) -> AppResult<Option<CacheEntry>> {
        let target = match (&row.storage_path, &row.digest) {
            (Some(_), _) => row,
            (None, Some(digest)) => {
                let key = s.store_key(a, digest);
                match self
                    .cache
                    .entry(row.repository_id, key.kind, &key.key, now)
                    .await?
                {
                    Some(target) => target,
                    None => return Ok(None),
                }
            }
            (None, None) => return Ok(None),
        };
        if !s
            .expected_digests(a, &HeaderMap::new())
            .admits_stored_sha256(target.digest.as_deref())
        {
            return Ok(None);
        }
        let Some(path) = target.storage_path.as_deref() else {
            return Ok(None);
        };
        Ok(self.storage.head(path).await?.map(|_| target))
    }

    // An untouched pointer would be evicted under a hot tag.
    async fn hit(
        &self,
        row: &CacheEntry,
        target: CacheEntry,
        now: DateTime<Utc>,
    ) -> AppResult<Cached> {
        self.cache.touch(row.id, None, now).await?;
        if target.id != row.id {
            self.cache.touch(target.id, None, now).await?;
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
            let known = s
                .expected_digests(a, &HeaderMap::new())
                .known(DigestAlgorithm::Sha256)
                .map(String::from);
            return Ok(Outcome::Found(Payload::head_only(size, content_type, known)));
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

    fn ttl(&self, policy: CachePolicy) -> Option<Duration> {
        self.ttl_secs(policy).map(Duration::from_secs)
    }
}

/// The digests the upstream announced before any request, as one key: a
/// quarantine holds while they do not change.
fn announced<S: UpstreamStrategy>(s: &S, a: &S::Artifact) -> Option<String> {
    let expected = s.expected_digests(a, &HeaderMap::new());
    let mut known: Vec<String> = expected
        .entries()
        .iter()
        .filter(|d| d.source == DigestSource::Known)
        .map(|d| format!("{:?}:{}", d.algorithm, d.value).to_ascii_lowercase())
        .collect();
    known.sort();
    (!known.is_empty()).then(|| known.join(","))
}

#[cfg(test)]
mod tests;
