use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use sqlx::SqlitePool;

use super::*;
use crate::db::Repository;
use crate::proxy::strategy::{
    CacheKey, CachePolicy, Transfer, Ttl, UrlSource, DEFAULT_MAX_UPSTREAM_BYTES,
};
use crate::storage::FilesystemStorage;

#[derive(Default)]
pub(super) struct FakeState {
    pub hits: Vec<(Method, String, HeaderMap)>,
    pub fail: bool,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub delay: Duration,
}

pub(super) type Shared = Arc<Mutex<FakeState>>;

async fn serve(State(st): State<Shared>, req: Request) -> Response {
    let (method, path, headers) = (
        req.method().clone(),
        req.uri().path().to_string(),
        req.headers().clone(),
    );
    let (fail, body, etag, delay) = {
        let mut s = st.lock().unwrap();
        s.hits.push((method, path.clone(), headers.clone()));
        (s.fail, s.body.clone(), s.etag.clone(), s.delay)
    };
    if fail {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if path.ends_with("/missing") {
        return StatusCode::NOT_FOUND.into_response();
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

pub(super) struct Strat {
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

pub(super) fn pointer_strat() -> Strat {
    Strat {
        pointer: true,
        ..Default::default()
    }
}

pub(super) struct Fx {
    _tmp: tempfile::TempDir,
    pub pool: SqlitePool,
    pub storage: Arc<FilesystemStorage>,
    pub repo: Repository,
    pub fake: Shared,
    pub up: Upstream,
}

pub(super) fn timeouts() -> Timeouts {
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
        crate::db::migrate(&pool).await.unwrap();
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
        let storage = Arc::new(FilesystemStorage::new(tmp.path().join("storage")));
        let up = Upstream {
            base,
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        };
        Self {
            _tmp: tmp,
            pool,
            storage,
            repo,
            fake,
            up,
        }
    }

    pub fn engine(&self, timeouts: Timeouts) -> ProxyEngine {
        let ttl = TtlConfig {
            default_secs: 3600,
            negative_secs: 600,
        };
        ProxyEngine::new(self.storage.clone(), self.pool.clone(), timeouts, ttl)
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

    pub async fn expire(&self) {
        sqlx::query("UPDATE proxy_cache_entries SET expires_at = datetime('now', '-1 second') WHERE expires_at IS NOT NULL")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    pub async fn row(&self, kind: &str, key: &str) -> Option<CacheEntry> {
        proxy_cache::get_entry(&self.pool, self.repo.id, kind, key)
            .await
            .unwrap()
            .map(|(r, _)| r)
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
