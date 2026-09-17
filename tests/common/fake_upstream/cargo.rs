use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::json;
use sha2::Digest;

use opencargo::registry::cargo::compute_prefix;

const ETAG: &str = "\"fake-index-v1\"";

/// One recorded request: its path with query and the `If-None-Match` and
/// `Authorization` it carried.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub if_none_match: Option<String>,
    pub authorization: Option<String>,
}

/// One `/api/v1/crates/{name}/{version}` request: when it started and
/// ended, and the `User-Agent` it carried.
#[derive(Clone, Debug)]
pub struct ApiHit {
    pub path: String,
    pub started: Instant,
    pub ended: Instant,
    pub user_agent: Option<String>,
}

#[derive(Default)]
struct Script {
    dl: String,
    lines: HashMap<String, Vec<String>>,
    files: Vec<(String, String, Vec<u8>)>,
    created_at: HashMap<(String, String), String>,
    api_status: u16,
    api_latency: Duration,
}

#[derive(Clone)]
struct Shared {
    hits: Arc<Mutex<Vec<Hit>>>,
    api_hits: Arc<Mutex<Vec<ApiHit>>>,
    script: Arc<Mutex<Script>>,
}

/// A crates.io-shaped sparse index whose `config.json` `dl`, index lines and
/// download bytes the test scripts: what a second opencargo cannot emulate
/// (a `dl` template with markers or a private literal, a wrong cksum, an
/// ETag answered 304).
pub struct FakeIndex {
    pub base_url: String,
    shared: Shared,
}

impl FakeIndex {
    pub fn index_url(&self) -> String {
        format!("{}/index", self.base_url)
    }

    /// `/index/{prefix}/{name}` as cargo requests it.
    pub fn index_path(&self, name: &str) -> String {
        format!("/index/{}/{}", compute_prefix(name), name.to_lowercase())
    }

    /// The `dl` template served in `config.json`; `tail` is appended to
    /// `{base}/dl` and may carry cargo's markers.
    pub fn set_dl(&self, tail: &str) {
        self.shared.script.lock().unwrap().dl = format!("{}/dl{tail}", self.base_url);
    }

    /// Serve any absolute `dl`, such as one pointing off this fake.
    pub fn set_dl_absolute(&self, dl: &str) {
        self.shared.script.lock().unwrap().dl = dl.to_string();
    }

    /// Index `name@version` with the true checksum of `bytes`; returns it.
    pub fn add_crate(&self, name: &str, version: &str, bytes: &[u8]) -> String {
        let cksum = format!("{:x}", sha2::Sha256::digest(bytes));
        self.add_crate_with_cksum(name, version, bytes, &cksum);
        cksum
    }

    /// Index `name@version` under a scripted checksum, true or not.
    pub fn add_crate_with_cksum(&self, name: &str, version: &str, bytes: &[u8], cksum: &str) {
        let line = json!({
            "name": name,
            "vers": version,
            "deps": [],
            "cksum": cksum,
            "features": {},
            "yanked": false,
        })
        .to_string();
        let mut script = self.shared.script.lock().unwrap();
        script
            .lines
            .entry(name.to_lowercase())
            .or_default()
            .push(line);
        script
            .files
            .push((name.to_string(), version.to_string(), bytes.to_vec()));
    }

    /// `GET /api/v1/crates/{name}/{version}` answers `version.created_at`.
    pub fn set_created_at(&self, name: &str, version: &str, created_at: &str) {
        self.shared.script.lock().unwrap().created_at.insert(
            (name.to_string(), version.to_string()),
            created_at.to_string(),
        );
    }

    /// Every version API request answers this status instead (200 restores).
    pub fn set_api_status(&self, status: u16) {
        self.shared.script.lock().unwrap().api_status = status;
    }

    pub fn set_api_latency(&self, latency: Duration) {
        self.shared.script.lock().unwrap().api_latency = latency;
    }

    pub fn api_hits(&self) -> Vec<ApiHit> {
        self.shared.api_hits.lock().unwrap().clone()
    }

    /// An index file for `name` that lists no version at all.
    pub fn add_empty_index(&self, name: &str) {
        self.shared
            .script
            .lock()
            .unwrap()
            .lines
            .entry(name.to_lowercase())
            .or_default();
    }

    pub fn count(&self, path: &str) -> usize {
        self.hits().filter(|h| h.path == path).count()
    }

    /// Requests on `path` that carried the current ETag and were answered 304.
    pub fn revalidations(&self, path: &str) -> usize {
        self.hits()
            .filter(|h| h.path == path && h.if_none_match.as_deref() == Some(ETAG))
            .count()
    }

    /// The `Authorization` the last request on `path` carried, if any.
    pub fn authorization(&self, path: &str) -> Option<String> {
        self.hits()
            .filter(|h| h.path == path)
            .last()
            .and_then(|h| h.authorization)
    }

    /// Every download request path, in order.
    pub fn downloads(&self) -> Vec<String> {
        self.hits()
            .filter(|h| h.path.starts_with("/dl"))
            .map(|h| h.path)
            .collect()
    }

    fn hits(&self) -> impl Iterator<Item = Hit> {
        self.shared.hits.lock().unwrap().clone().into_iter()
    }
}

pub async fn start() -> FakeIndex {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake index");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{addr}");
    let shared = Shared {
        hits: Arc::new(Mutex::new(Vec::new())),
        api_hits: Arc::new(Mutex::new(Vec::new())),
        script: Arc::new(Mutex::new(Script {
            dl: format!("{base_url}/dl"),
            api_status: 200,
            ..Default::default()
        })),
    };
    let app = Router::new().fallback(serve).with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeIndex { base_url, shared }
}

async fn serve(State(shared): State<Shared>, req: Request) -> Response {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers = req.headers().clone();
    let header_value = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let if_none_match = header_value(header::IF_NONE_MATCH);
    shared.hits.lock().unwrap().push(Hit {
        path: path.clone(),
        if_none_match: if_none_match.clone(),
        authorization: header_value(header::AUTHORIZATION),
    });
    if let Some(rest) = path.strip_prefix("/api/v1/crates/") {
        return version_api(&shared, &path, rest, header_value(header::USER_AGENT)).await;
    }
    let script = shared.script.lock().unwrap();
    if path == "/index/config.json" {
        let api = script.dl.trim_end_matches("/dl").to_string();
        return axum::Json(json!({ "dl": script.dl, "api": api })).into_response();
    }
    if let Some(name) = path
        .strip_prefix("/index/")
        .and_then(|p| p.rsplit('/').next())
    {
        let expected = format!("/index/{}/{name}", compute_prefix(name));
        return match script.lines.get(name) {
            Some(lines) if path == expected => index_response(lines, if_none_match.as_deref()),
            _ => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if path.starts_with("/dl") {
        let found = script.files.iter().find(|(name, version, _)| {
            path.contains(&format!("/{name}/{version}/"))
                || path.contains(&format!("{name}-{version}.crate"))
        });
        return match found {
            Some((_, _, bytes)) => {
                ([(header::CONTENT_TYPE, "application/gzip")], bytes.clone()).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    StatusCode::NOT_FOUND.into_response()
}

/// `{name}/{version}` -> `{"version": {"created_at": ..}}`, paced by the
/// scripted latency and recorded with its start and end.
async fn version_api(
    shared: &Shared,
    path: &str,
    rest: &str,
    user_agent: Option<String>,
) -> Response {
    let started = Instant::now();
    let (status, created_at, latency) = {
        let script = shared.script.lock().unwrap();
        let created_at = rest
            .split_once('/')
            .and_then(|(name, version)| {
                script
                    .created_at
                    .get(&(name.to_string(), version.to_string()))
            })
            .cloned();
        (script.api_status, created_at, script.api_latency)
    };
    tokio::time::sleep(latency).await;
    shared.api_hits.lock().unwrap().push(ApiHit {
        path: path.to_string(),
        started,
        ended: Instant::now(),
        user_agent,
    });
    let status = StatusCode::from_u16(status).expect("valid status");
    match (status, created_at, rest.split_once('/')) {
        (StatusCode::OK, Some(created_at), Some((name, version))) => axum::Json(json!({
            "version": { "crate": name, "num": version, "created_at": created_at }
        }))
        .into_response(),
        (StatusCode::OK, _, _) => StatusCode::NOT_FOUND.into_response(),
        (other, _, _) => other.into_response(),
    }
}

fn index_response(lines: &[String], if_none_match: Option<&str>) -> Response {
    if if_none_match == Some(ETAG) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, ETAG)
            .body(Body::empty())
            .expect("valid 304");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, ETAG)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(lines.join("\n")))
        .expect("valid index response")
}
