//! A Verdaccio-shaped npm registry: `/-/v1/search`, full packuments with no
//! tarball size, tarballs, and the web data route when asked for.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::{serve, sha1_hex, Log};

#[derive(Clone, Debug)]
pub struct Ver {
    pub version: String,
    pub tarball: Vec<u8>,
    pub deps: Value,
    /// Announce this shasum instead of the real one.
    pub shasum: Option<String>,
    /// Append this query string to the announced tarball URL.
    pub tarball_query: Option<String>,
    pub time: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Pkg {
    pub name: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub versions: Vec<Ver>,
    pub dist_tags: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    Honest,
    /// Answers `total: 2` whatever the registry holds.
    TotalLie,
    /// Serves offset `n` for every `from` above it, as Verdaccio 6.8 does.
    ClampAt(usize),
    /// Answers no objects for an empty query.
    Empty,
}

pub struct Config {
    pub search: Search,
    /// `Some(list)`: the web data route answers; `None`: it 404s.
    pub web_data: Option<bool>,
    pub powered_by: Option<String>,
    /// `p0` .. `pN`, one version each, generated on demand.
    pub synthetic: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { search: Search::Honest, web_data: None, powered_by: Some("verdaccio/6.1.2".into()), synthetic: 0 }
    }
}

pub struct Inner {
    pub pkgs: Mutex<BTreeMap<String, Pkg>>,
    pub config: Config,
    pub log: Log,
    pub base: Mutex<String>,
}

#[derive(Clone)]
pub struct FakeVerdaccio {
    pub url: String,
    pub inner: Arc<Inner>,
}

pub fn tarball(name: &str, version: &str, deps: &Value, readme: Option<&str>) -> Vec<u8> {
    tarball_padded(name, version, deps, readme, 0)
}

/// A tarball with `pad` incompressible bytes, for size boundaries.
pub fn tarball_padded(name: &str, version: &str, deps: &Value, readme: Option<&str>, pad: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let manifest = json!({ "name": name, "version": version, "dependencies": deps, "main": "index.js" }).to_string();
        let mut files: Vec<(&str, Vec<u8>)> = vec![("package/package.json", manifest.into_bytes())];
        if let Some(r) = readme {
            files.push(("package/README.md", r.as_bytes().to_vec()));
        }
        if pad > 0 {
            let mut x: u32 = 2463534242;
            let noise: Vec<u8> = (0..pad)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            files.push(("package/blob.bin", noise));
        }
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append(&h, data.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
    }
    buf
}

impl Ver {
    pub fn new(name: &str, version: &str) -> Self {
        Self::with_deps(name, version, json!({}))
    }

    pub fn with_deps(name: &str, version: &str, deps: Value) -> Self {
        Self {
            version: version.into(),
            tarball: tarball(name, version, &deps, None),
            deps,
            shasum: None,
            tarball_query: None,
            time: None,
        }
    }
}

impl Pkg {
    pub fn new(name: &str, versions: &[&str]) -> Self {
        Self {
            name: name.into(),
            description: Some(format!("the {name} package")),
            readme: Some(format!("# {name}\n")),
            versions: versions.iter().map(|v| Ver::new(name, v)).collect(),
            dist_tags: versions.last().map(|v| vec![("latest".to_string(), v.to_string())]).unwrap_or_default(),
        }
    }
}

fn short(name: &str) -> &str {
    name.split_once('/').map_or(name, |(_, n)| n)
}

impl Inner {
    fn packument(&self, name: &str) -> Option<Value> {
        let base = self.base.lock().unwrap().clone();
        if let Some(i) = name.strip_prefix('p').and_then(|n| n.parse::<usize>().ok()) {
            if i < self.config.synthetic && !self.pkgs.lock().unwrap().contains_key(name) {
                return Some(json!({ "name": name, "dist-tags": { "latest": "1.0.0" },
                    "versions": { "1.0.0": { "name": name, "version": "1.0.0",
                        "dist": { "shasum": "00", "tarball": format!("{base}/{name}/-/{name}-1.0.0.tgz") } } } }));
            }
        }
        let pkgs = self.pkgs.lock().unwrap();
        let p = pkgs.get(name)?;
        let mut versions = serde_json::Map::new();
        let mut time = serde_json::Map::new();
        for v in &p.versions {
            let mut url = format!("{base}/{}/-/{}-{}.tgz", p.name, short(&p.name), v.version);
            if let Some(q) = &v.tarball_query {
                url = format!("{url}?{q}");
            }
            versions.insert(
                v.version.clone(),
                json!({
                    "name": p.name,
                    "version": v.version,
                    "description": p.description,
                    "dependencies": v.deps,
                    "license": "MIT",
                    "dist": { "shasum": v.shasum.clone().unwrap_or_else(|| sha1_hex(&v.tarball)), "tarball": url }
                }),
            );
            time.insert(v.version.clone(), json!(v.time.clone().unwrap_or_else(|| "2026-01-01T00:00:00.000Z".into())));
        }
        let tags: serde_json::Map<String, Value> = p.dist_tags.iter().map(|(t, v)| (t.clone(), json!(v))).collect();
        let mut doc = json!({ "name": p.name, "dist-tags": tags, "versions": versions, "time": time });
        if let Some(d) = &p.description {
            doc["description"] = json!(d);
        }
        if let Some(r) = &p.readme {
            doc["readme"] = json!(r);
        }
        Some(doc)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.pkgs.lock().unwrap().keys().cloned().collect();
        names.extend((0..self.config.synthetic).map(|i| format!("p{i}")));
        names
    }

    fn tarball(&self, name: &str, file: &str) -> Option<Vec<u8>> {
        let pkgs = self.pkgs.lock().unwrap();
        let p = pkgs.get(name)?;
        p.versions.iter().find(|v| format!("{}-{}.tgz", short(&p.name), v.version) == file).map(|v| v.tarball.clone())
    }
}

#[derive(serde::Deserialize)]
struct SearchQ {
    text: Option<String>,
    size: Option<usize>,
    from: Option<usize>,
}

async fn search(State(s): State<Arc<Inner>>, Query(q): Query<SearchQ>) -> Response {
    let size = q.size.unwrap_or(20);
    let mut from = q.from.unwrap_or(0);
    if s.config.search == Search::Empty && q.text.as_deref().unwrap_or("").is_empty() {
        return Json(json!({ "objects": [], "total": 0 })).into_response();
    }
    if let Search::ClampAt(n) = s.config.search {
        from = from.min(n);
    }
    let names = s.names();
    let page: Vec<Value> = names
        .iter()
        .skip(from)
        .take(size)
        .map(|n| json!({ "package": { "name": n, "version": "1.0.0" } }))
        .collect();
    let total = if s.config.search == Search::TotalLie { 2 } else { names.len() };
    let mut resp = Json(json!({ "objects": page, "total": total })).into_response();
    if let Some(p) = &s.config.powered_by {
        resp.headers_mut().insert("x-powered-by", p.parse().unwrap());
    }
    resp
}

async fn fallback(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let path = percent_decode(req.uri().path().trim_start_matches('/'));
    if path == "-/v1/search" {
        let q = Query::<SearchQ>::try_from_uri(req.uri()).unwrap();
        return search(State(s.clone()), q).await;
    }
    if path == "-/verdaccio/data/packages" {
        return match s.config.web_data {
            Some(true) => Json(s.names().iter().map(|n| json!({ "name": n })).collect::<Vec<_>>()).into_response(),
            Some(false) => Json(json!([])).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if path == "-/whoami" {
        return Json(json!({ "username": "importer" })).into_response();
    }
    if let Some((name, file)) = path.split_once("/-/") {
        return match s.tarball(name, file) {
            Some(b) => b.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    match s.packument(&path) {
        Some(doc) => Json(doc).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    s.replace("%2f", "/").replace("%2F", "/").replace("%40", "@")
}

impl FakeVerdaccio {
    pub async fn start(pkgs: Vec<Pkg>, config: Config) -> Self {
        let inner = Arc::new(Inner {
            pkgs: Mutex::new(pkgs.into_iter().map(|p| (p.name.clone(), p)).collect()),
            config,
            log: Log::default(),
            base: Mutex::new(String::new()),
        });
        let router = axum::Router::new().fallback(fallback).with_state(inner.clone());
        let url = serve(router).await;
        *inner.base.lock().unwrap() = url.clone();
        Self { url, inner }
    }

    pub fn log(&self) -> &Log {
        &self.inner.log
    }

    pub fn set(&self, pkg: Pkg) {
        self.inner.pkgs.lock().unwrap().insert(pkg.name.clone(), pkg);
    }
}
