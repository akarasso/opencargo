use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::SqlitePool;

use crate::domain::{CacheEntry, CacheEntryId, NewEntry, RepoId, Repository};
use crate::error::{AppResult, StoreError};
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::proxy::engine::{ProxyEngine, Timeouts, TtlConfig};
use crate::proxy::strategy::{
    CacheKey, CachePolicy, Transfer, Ttl, UpstreamStrategy, UrlSource, DEFAULT_MAX_UPSTREAM_BYTES,
};
use crate::registry::resolve::{CacheRepo, Upstream};
use crate::storage::StorageBackend;

#[derive(Default)]
pub(crate) struct FakeState {
    pub hits: Vec<(Method, String, HeaderMap)>,
    pub starts: Vec<Instant>,
    pub inflight: usize,
    pub max_inflight: usize,
    pub fail: bool,
    pub gone: bool,
    pub status: Option<StatusCode>,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub delay: Duration,
}

pub(crate) type Shared = Arc<Mutex<FakeState>>;

async fn serve(State(st): State<Shared>, req: Request) -> Response {
    {
        let mut s = st.lock().unwrap();
        s.inflight += 1;
        s.max_inflight = s.max_inflight.max(s.inflight);
    }
    let response = respond(st.clone(), req).await;
    st.lock().unwrap().inflight -= 1;
    response
}

async fn respond(st: Shared, req: Request) -> Response {
    let (method, path, headers) = (
        req.method().clone(),
        req.uri().path().to_string(),
        req.headers().clone(),
    );
    let (fail, gone, status, body, etag, delay) = {
        let mut s = st.lock().unwrap();
        s.hits.push((method, path.clone(), headers.clone()));
        s.starts.push(Instant::now());
        (
            s.fail,
            s.gone,
            s.status,
            s.body.clone(),
            s.etag.clone(),
            s.delay,
        )
    };
    if fail {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if let Some(status) = status {
        return status.into_response();
    }
    if gone || path.ends_with("/missing") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if path.ends_with("/lying") {
        return Response::builder()
            .header(header::CONTENT_LENGTH, "100")
            .body(Body::from("five!"))
            .unwrap();
    }
    if path.ends_with("/drip") {
        let stream = futures_util::stream::unfold(0u8, |n| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            (n < 5).then(|| {
                (
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"drip")),
                    n + 1,
                )
            })
        });
        return Response::new(Body::from_stream(stream));
    }
    if etag.is_some()
        && headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            == etag.as_deref()
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    tokio::time::sleep(delay).await;
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    if let Some(etag) = etag {
        resp.headers_mut()
            .insert(header::ETAG, etag.parse().unwrap());
    }
    resp
}

pub(crate) struct Strat {
    pub transfer: Transfer,
    pub policy: CachePolicy,
    pub pointer: bool,
    pub max: u64,
    pub via_get: bool,
    pub source: UrlSource,
}

impl Default for Strat {
    fn default() -> Self {
        Self {
            transfer: Transfer::Buffered,
            policy: CachePolicy::Ttl(Ttl::Default),
            pointer: false,
            max: DEFAULT_MAX_UPSTREAM_BYTES,
            via_get: true,
            source: UrlSource::Admin,
        }
    }
}

impl UpstreamStrategy for Strat {
    type Artifact = String;

    fn upstream_url(&self, up: &Upstream, a: &String) -> AppResult<reqwest::Url> {
        Ok(up.base.join(a).unwrap())
    }

    fn url_source(&self, _up: &Upstream, _a: &String) -> UrlSource {
        self.source
    }

    fn cache_key(&self, a: &String) -> CacheKey {
        CacheKey {
            kind: "t-item",
            key: a.clone(),
        }
    }

    fn store_key(&self, a: &String, sha: &str) -> CacheKey {
        if self.pointer {
            CacheKey {
                kind: "t-body",
                key: format!("sha256/{sha}"),
            }
        } else {
            self.cache_key(a)
        }
    }

    fn cache_policy(&self, _a: &String) -> CachePolicy {
        self.policy
    }

    fn transfer(&self, _a: &String) -> Transfer {
        self.transfer
    }

    fn max_bytes(&self, _a: &String) -> u64 {
        self.max
    }

    fn head_via_get(&self, _a: &String) -> bool {
        self.via_get
    }
}

pub(crate) fn pointer_strat() -> Strat {
    Strat {
        pointer: true,
        ..Default::default()
    }
}

/// Every instant the engine hands the store, moved forward by whatever the
/// test has advanced.
///
/// The engine reads the wall clock, and a test cannot wait an hour for a ttl
/// to run out. Shifting at the port moves the writing clock and the reading
/// clock together, so a row written before the advance is judged against the
/// same timeline as one written after it — which is what makes expiry
/// assertable with no sleep and no zero ttl.
struct Shifted {
    inner: Arc<dyn ProxyCacheStore>,
    seconds: Arc<AtomicI64>,
}

impl Shifted {
    fn at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + TimeDelta::seconds(self.seconds.load(Ordering::Relaxed))
    }
}

#[async_trait]
impl ProxyCacheStore for Shifted {
    async fn entry(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheEntry>, StoreError> {
        self.inner.entry(repo, kind, key, self.at(now)).await
    }

    async fn upsert(&self, entry: &NewEntry<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.upsert(entry, self.at(now)).await
    }

    async fn touch(
        &self,
        id: CacheEntryId,
        ttl: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.inner.touch(id, ttl, self.at(now)).await
    }

    async fn delete_for_repo(&self, repo: RepoId) -> Result<u64, StoreError> {
        self.inner.delete_for_repo(repo).await
    }

    async fn evictable(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CacheEntry>, StoreError> {
        self.inner.evictable(idle, self.at(now), limit).await
    }

    async fn delete(&self, id: CacheEntryId) -> Result<(), StoreError> {
        self.inner.delete(id).await
    }
}

pub(crate) struct Fx {
    _tmp: tempfile::TempDir,
    pub pool: SqlitePool,
    pub cache: Arc<dyn ProxyCacheStore>,
    pub storage: Arc<dyn StorageBackend>,
    pub repo: Repository,
    pub fake: Shared,
    pub up: Upstream,
    clock: Arc<AtomicI64>,
}

pub(crate) fn timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(1),
        read_idle: Duration::from_secs(5),
        buffered_total: Duration::from_secs(5),
        singleflight_wait: Duration::from_secs(5),
    }
}

impl Fx {
    pub async fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tmp = tempfile::TempDir::new().unwrap();
        let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::server::migrate(&pool).await.unwrap();
        let fake: Shared = Arc::new(Mutex::new(FakeState {
            body: b"hello upstream".to_vec(),
            ..Default::default()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let app = axum::Router::new().fallback(serve).with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        sqlx::query("INSERT INTO repositories (name, repo_type, format, upstream_url) VALUES ('p', 'proxy', 'npm', ?1)")
            .bind(base.as_str())
            .execute(&pool)
            .await
            .unwrap();
        let repo = crate::db::get_repository_by_name(&pool, "p")
            .await
            .unwrap()
            .unwrap();
        let storage = crate::storage::filesystem(tmp.path().join("storage"));
        let clock = Arc::new(AtomicI64::new(0));
        let cache: Arc<dyn ProxyCacheStore> = Arc::new(Shifted {
            inner: crate::server::proxy_cache_store(&pool),
            seconds: clock.clone(),
        });
        let up = Upstream {
            base,
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        };
        Self {
            _tmp: tmp,
            pool,
            cache,
            storage,
            repo,
            fake,
            up,
            clock,
        }
    }

    pub fn engine(&self, timeouts: Timeouts) -> ProxyEngine {
        let ttl = TtlConfig {
            default_secs: 3600,
            negative_secs: 600,
        };
        ProxyEngine::new(self.storage.clone(), self.cache.clone(), timeouts, ttl)
    }

    pub fn member(&self) -> CacheRepo<'_> {
        CacheRepo(&self.repo)
    }

    pub fn hits(&self) -> Vec<(Method, String, HeaderMap)> {
        self.fake.lock().unwrap().hits.clone()
    }

    pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.fake.lock().unwrap());
    }

    /// Move the clock every cache read and write sees forward.
    pub fn advance(&self, by: Duration) {
        self.clock
            .fetch_add(by.as_secs() as i64, Ordering::Relaxed);
    }

    /// Past the fixture's longest ttl, so every row that has one is stale and
    /// every immutable row is untouched -- what a day of traffic would do.
    pub fn expire(&self) {
        self.advance(Duration::from_secs(3601));
    }

    pub async fn row(&self, kind: &str, key: &str) -> Option<CacheEntry> {
        self.cache
            .entry(self.repo.id, kind, key, Utc::now())
            .await
            .unwrap()
    }

    pub fn files(&self) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut pending = vec![self.storage.resolve("").unwrap()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if entry.path().is_dir() {
                    pending.push(entry.path())
                } else {
                    out.push(entry.path())
                }
            }
        }
        out
    }
}
