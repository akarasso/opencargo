//! The GitHub Packages REST listing: packages per owner and type, container
//! versions with their tags. The bytes live elsewhere: an npm fake and a
//! spawned opencargo standing in for the container registry.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::{serve, Log};

/// `(digest, tags)` of one container version.
pub type Version = (String, Vec<String>);

pub struct Inner {
    pub owner: String,
    pub npm: Vec<String>,
    pub containers: Vec<(String, Vec<Version>)>,
    pub maven: usize,
    pub log: Log,
}

#[derive(Clone)]
pub struct FakeGithub {
    pub url: String,
    pub inner: Arc<Inner>,
}

fn page<T: Clone>(all: &[T], per_page: usize, page: usize) -> Vec<T> {
    all.iter().skip((page.max(1) - 1) * per_page).take(per_page).cloned().collect()
}

async fn handle(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let path = req.uri().path().replace("%2F", "/");
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned().collect();
    let q = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let per_page: usize = q("per_page").and_then(|v| v.parse().ok()).unwrap_or(30);
    let pg: usize = q("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    if per_page * pg > 10_000 {
        return (StatusCode::UNPROCESSABLE_ENTITY, "per_page * page > 10000").into_response();
    }
    if path == "/user" {
        return Json(json!({ "login": "importer" })).into_response();
    }
    let prefix = format!("/orgs/{}/packages", s.owner);
    let Some(rest) = path.strip_prefix(&prefix) else { return StatusCode::NOT_FOUND.into_response() };
    if rest.is_empty() {
        let names: Vec<String> = match q("package_type").as_deref() {
            Some("npm") => s.npm.clone(),
            Some("container") => s.containers.iter().map(|(n, _)| n.clone()).collect(),
            Some("maven") => (0..s.maven).map(|i| format!("com.example.m{i}")).collect(),
            _ => Vec::new(),
        };
        let list: Vec<Value> = page(&names, per_page, pg).into_iter().map(|n| json!({ "name": n })).collect();
        return Json(list).into_response();
    }
    if let Some(name) = rest.strip_prefix("/container/").and_then(|r| r.strip_suffix("/versions")) {
        let Some((_, versions)) = s.containers.iter().find(|(n, _)| n == name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let list: Vec<Value> = page(versions, per_page, pg)
            .into_iter()
            .map(|(d, tags)| json!({ "name": d, "metadata": { "container": { "tags": tags } } }))
            .collect();
        return Json(list).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

impl FakeGithub {
    pub async fn start(inner: Inner) -> Self {
        let inner = Arc::new(inner);
        let url = serve(axum::Router::new().fallback(handle).with_state(inner.clone())).await;
        Self { url, inner }
    }
}

impl Inner {
    pub fn new(owner: &str) -> Self {
        Self { owner: owner.into(), npm: Vec::new(), containers: Vec::new(), maven: 0, log: Log::default() }
    }
}
