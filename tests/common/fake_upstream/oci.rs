//! A fake OCI registry for what a second opencargo cannot emulate: a Bearer
//! challenge (on HEAD too), a token realm that may demand Basic, a Hub-shaped
//! 401 for an unknown name, ETag/304, and blobs that are huge, slow or lying.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use sha2::Digest;

pub const PATTERN_CHUNK: usize = 1024 * 1024;
const TRICKLE_CHUNK: &[u8] = b"drip";

#[derive(Clone, Debug)]
pub struct Hit {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
}

#[derive(Default)]
pub struct Options {
    /// Answer 401 with a Bearer challenge until a token from the realm is sent.
    pub challenge: bool,
    /// The realm the challenge names; default `{base_url}/token`.
    pub realm: Option<String>,
    /// `/token` refuses anything but these Basic credentials.
    pub realm_basic: Option<(String, String)>,
    /// After a token, an unknown name is 401 UNAUTHORIZED (Hub) instead of 404.
    pub hub_shape: bool,
}

pub enum Blob {
    Bytes(Vec<u8>),
    /// `size` bytes of a fixed pattern, streamed in 1 MiB chunks.
    Pattern {
        size: usize,
    },
    /// `chunks` times `b"drip"`, one every `every`.
    Trickle {
        chunks: u32,
        every: Duration,
    },
}

impl Blob {
    fn size(&self) -> usize {
        match self {
            Blob::Bytes(b) => b.len(),
            Blob::Pattern { size } => *size,
            Blob::Trickle { chunks, .. } => *chunks as usize * TRICKLE_CHUNK.len(),
        }
    }

    /// The digest of the bytes a blob will stream.
    pub fn digest(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        match self {
            Blob::Bytes(b) => hasher.update(b),
            Blob::Pattern { size } => {
                let chunk = pattern_chunk();
                let mut left = *size;
                while left > 0 {
                    let n = left.min(PATTERN_CHUNK);
                    hasher.update(&chunk[..n]);
                    left -= n;
                }
            }
            Blob::Trickle { chunks, .. } => {
                for _ in 0..*chunks {
                    hasher.update(TRICKLE_CHUNK);
                }
            }
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    fn body(&self) -> Body {
        match self {
            Blob::Bytes(b) => Body::from(b.clone()),
            Blob::Pattern { size } => {
                let chunk = Arc::new(pattern_chunk());
                let stream = futures_util::stream::unfold(*size, move |left| {
                    let chunk = chunk.clone();
                    async move {
                        (left > 0).then(|| {
                            let n = left.min(PATTERN_CHUNK);
                            let bytes = bytes::Bytes::copy_from_slice(&chunk[..n]);
                            (Ok::<_, std::io::Error>(bytes), left - n)
                        })
                    }
                });
                Body::from_stream(stream)
            }
            Blob::Trickle { chunks, every } => {
                let every = *every;
                let stream = futures_util::stream::unfold(*chunks, move |left| async move {
                    if left == 0 {
                        return None;
                    }
                    tokio::time::sleep(every).await;
                    let bytes = bytes::Bytes::from_static(TRICKLE_CHUNK);
                    Some((Ok::<_, std::io::Error>(bytes), left - 1))
                });
                Body::from_stream(stream)
            }
        }
    }
}

fn pattern_chunk() -> Vec<u8> {
    (0..PATTERN_CHUNK).map(|i| (i * 31 + 7) as u8).collect()
}

#[derive(Default)]
struct Image {
    manifests: HashMap<String, (Vec<u8>, String)>,
    tags: HashMap<String, String>,
    blobs: HashMap<String, Arc<Blob>>,
}

#[derive(Default)]
struct Registry {
    images: HashMap<String, Image>,
    hits: Vec<Hit>,
    tokens_issued: usize,
}

type Shared = Arc<Mutex<Registry>>;

#[derive(Clone)]
struct App {
    reg: Shared,
    opts: Arc<Options>,
    realm: String,
}

pub struct FakeRegistry {
    pub base_url: String,
    reg: Shared,
}

impl FakeRegistry {
    pub fn add_manifest(
        &self,
        name: &str,
        tag: Option<&str>,
        bytes: &[u8],
        content_type: &str,
    ) -> String {
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(bytes));
        let mut reg = self.reg.lock().unwrap();
        let image = reg.images.entry(name.to_string()).or_default();
        image
            .manifests
            .insert(digest.clone(), (bytes.to_vec(), content_type.to_string()));
        if let Some(tag) = tag {
            image.tags.insert(tag.to_string(), digest.clone());
        }
        digest
    }

    /// Register `blob` under its own digest.
    pub fn add_blob(&self, name: &str, blob: Blob) -> String {
        let digest = blob.digest();
        self.add_blob_as(name, &digest, blob);
        digest
    }

    /// Register `blob` under `digest`, which may lie about the bytes.
    pub fn add_blob_as(&self, name: &str, digest: &str, blob: Blob) {
        let mut reg = self.reg.lock().unwrap();
        let image = reg.images.entry(name.to_string()).or_default();
        image.blobs.insert(digest.to_string(), Arc::new(blob));
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.reg.lock().unwrap().hits.clone()
    }

    pub fn count(&self, method: Method, path: &str) -> usize {
        self.hits()
            .iter()
            .filter(|h| h.method == method && h.path == path)
            .count()
    }

    pub fn tokens_issued(&self) -> usize {
        self.reg.lock().unwrap().tokens_issued
    }
}

pub async fn start(opts: Options) -> FakeRegistry {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake registry");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{addr}");
    let reg: Shared = Arc::default();
    let app = App {
        reg: reg.clone(),
        realm: opts
            .realm
            .clone()
            .unwrap_or_else(|| format!("{base_url}/token")),
        opts: Arc::new(opts),
    };
    let router = Router::new()
        .route("/token", get(token))
        .fallback(serve)
        .with_state(app);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    FakeRegistry { base_url, reg }
}

fn record(app: &App, req: &Request) {
    app.reg.lock().unwrap().hits.push(Hit {
        method: req.method().clone(),
        path: req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default(),
        headers: req.headers().clone(),
    });
}

async fn token(State(app): State<App>, req: Request) -> Response {
    record(&app, &req);
    if let Some((user, pass)) = &app.opts.realm_basic {
        let expected = format!("Basic {}", base64_encode(&format!("{user}:{pass}")));
        let sent = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if sent != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let mut reg = app.reg.lock().unwrap();
    reg.tokens_issued += 1;
    let token = format!("tok-{}", reg.tokens_issued);
    Json(json!({ "token": token, "expires_in": 300 })).into_response()
}

fn base64_encode(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}

enum Route {
    Manifest(String, String),
    Blob(String, String),
    Tags(String),
}

/// `/v2/{name}/manifests/{ref}`, `/v2/{name}/blobs/{digest}` or
/// `/v2/{name}/tags/list`, the name possibly nested.
fn route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v2/")?;
    if let Some(name) = rest.strip_suffix("/tags/list") {
        return Some(Route::Tags(name.to_string()));
    }
    let (head, last) = rest.rsplit_once('/')?;
    let (name, kind) = head.rsplit_once('/')?;
    match kind {
        "manifests" => Some(Route::Manifest(name.to_string(), last.to_string())),
        "blobs" => Some(Route::Blob(name.to_string(), last.to_string())),
        _ => None,
    }
}

async fn serve(State(app): State<App>, req: Request) -> Response {
    record(&app, &req);
    let path = req.uri().path().to_string();
    let Some(route) = route(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer tok-"));
    if app.opts.challenge && !bearer {
        let scope = match &route {
            Route::Manifest(name, _) | Route::Blob(name, _) | Route::Tags(name) => name,
        };
        let challenge = format!(
            r#"Bearer realm="{}",service="fake",scope="repository:{scope}:pull""#,
            app.realm
        );
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, challenge)],
            Json(json!({ "errors": [{ "code": "UNAUTHORIZED" }] })),
        )
            .into_response();
    }
    let head = req.method() == Method::HEAD;
    let if_none_match = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let reg = app.reg.lock().unwrap();
    let (name, found) = match &route {
        Route::Manifest(name, _) | Route::Blob(name, _) | Route::Tags(name) => {
            (name, reg.images.get(name))
        }
    };
    let Some(image) = found else {
        return unknown_name(&app, name);
    };
    match route {
        Route::Manifest(_, reference) => {
            let digest = image.tags.get(&reference).cloned().unwrap_or(reference);
            match image.manifests.get(&digest) {
                Some((bytes, content_type)) => {
                    manifest_response(bytes, content_type, &digest, head, if_none_match)
                }
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        Route::Blob(_, digest) => match image.blobs.get(&digest) {
            Some(blob) => blob_response(blob, &digest, head),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Route::Tags(name) => {
            let mut tags: Vec<&String> = image.tags.keys().collect();
            tags.sort();
            Json(json!({ "name": name, "tags": tags })).into_response()
        }
    }
}

fn unknown_name(app: &App, name: &str) -> Response {
    if app.opts.hub_shape {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "errors": [{ "code": "UNAUTHORIZED", "message": format!("unknown {name}") }] })),
        )
            .into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

fn manifest_response(
    bytes: &[u8],
    content_type: &str,
    digest: &str,
    head: bool,
    if_none_match: Option<String>,
) -> Response {
    let etag = format!("\"{digest}\"");
    if if_none_match.as_deref() == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut resp = Response::new(if head {
        Body::empty()
    } else {
        Body::from(bytes.to_vec())
    });
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    headers.insert(
        "docker-content-digest",
        HeaderValue::from_str(digest).unwrap(),
    );
    resp
}

fn blob_response(blob: &Blob, digest: &str, head: bool) -> Response {
    let mut resp = Response::new(if head { Body::empty() } else { blob.body() });
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(blob.size()));
    headers.insert(
        "docker-content-digest",
        HeaderValue::from_str(digest).unwrap(),
    );
    resp
}
