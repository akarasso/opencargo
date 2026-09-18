//! Repository managers: a Nexus 3 and an Artifactory, each serving its
//! listing API plus the npm endpoint, sparse cargo index and GOPROXY its
//! repositories expose.

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::verdaccio::{packument, tarball_of, Pkg};
use super::{serve, sha1_hex, sha256_hex, Log};

#[derive(Clone, Debug)]
pub struct Crate {
    pub name: String,
    pub version: String,
    pub bytes: Vec<u8>,
    pub line: Value,
}

/// A `.crate`: `{name}-{version}/Cargo.toml` (when given) and a README.
pub fn crate_file(name: &str, version: &str, manifest: Option<&str>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut files = vec![(format!("{name}-{version}/src/lib.rs"), b"pub fn f() {}\n".to_vec())];
        if let Some(m) = manifest {
            files.push((format!("{name}-{version}/Cargo.toml"), m.as_bytes().to_vec()));
            files.push((format!("{name}-{version}/README.md"), format!("# {name}\n").into_bytes()));
        }
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_path(&path).unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append(&h, data.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
    }
    buf
}

impl Crate {
    /// A crate whose index line carries `line` (deps, features, ...) and
    /// whose manifest describes it.
    pub fn new(name: &str, version: &str, line: Value) -> Self {
        let manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"the {name} crate\"\nlicense = \"MIT OR Apache-2.0\"\nrepository = \"https://example.com/{name}\"\nauthors = [\"Ann <ann@example.com>\"]\n"
        );
        Self::with_manifest(name, version, line, Some(&manifest))
    }

    pub fn with_manifest(name: &str, version: &str, mut line: Value, manifest: Option<&str>) -> Self {
        let bytes = crate_file(name, version, manifest);
        line["name"] = json!(name);
        line["vers"] = json!(version);
        line["cksum"] = json!(sha256_hex(&bytes));
        for (k, d) in [("deps", json!([])), ("features", json!({})), ("yanked", json!(false))] {
            if line.get(k).is_none() {
                line[k] = d;
            }
        }
        Self { name: name.into(), version: version.into(), bytes, line }
    }
}

#[derive(Clone, Debug)]
pub struct GoMod {
    pub module: String,
    pub version: String,
    pub zip: Vec<u8>,
}

impl GoMod {
    pub fn new(module: &str, version: &str) -> Self {
        Self { module: module.into(), version: version.into(), zip: super::super::build_go_module_zip(module, version) }
    }
}

#[derive(Clone, Debug)]
pub enum Content {
    Npm(Vec<Pkg>),
    /// `index: false` serves no sparse index, only the files.
    Cargo { crates: Vec<Crate>, index: bool },
    Go(Vec<GoMod>),
    Raw(Vec<(String, Vec<u8>)>),
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub name: String,
    /// `hosted`, `proxy` or `group`, in Nexus's words.
    pub kind: String,
    pub content: Content,
}

impl Repo {
    pub fn hosted(name: &str, content: Content) -> Self {
        Self { name: name.into(), kind: "hosted".into(), content }
    }

    fn format(&self) -> &'static str {
        match &self.content {
            Content::Npm(_) => "npm",
            Content::Cargo { .. } => "cargo",
            Content::Go(_) => "go",
            Content::Raw(_) => "raw",
        }
    }
}

pub struct Inner {
    pub repos: Mutex<Vec<Repo>>,
    pub page: usize,
    pub log: Log,
    pub base: Mutex<String>,
    /// Artifactory only: whether `/api/npm/` answers.
    pub npm_api: bool,
    /// Fixed JSON answers by path, checked before anything else.
    pub canned: Mutex<Vec<(String, Value)>>,
}

fn canned(s: &Inner, path: &str) -> Option<Response> {
    s.canned.lock().unwrap().iter().find(|(p, _)| p == path).map(|(_, v)| Json(v.clone()).into_response())
}

#[derive(Clone)]
pub struct FakeManager {
    pub url: String,
    pub inner: Arc<Inner>,
}

impl FakeManager {
    pub fn log(&self) -> &Log {
        &self.inner.log
    }

    pub fn from(&self) -> String {
        format!("{}/", self.url)
    }

    pub fn answer(&self, path: &str, body: Value) {
        self.inner.canned.lock().unwrap().push((path.to_string(), body));
    }

    async fn start(repos: Vec<Repo>, page: usize, npm_api: bool, router: fn(Arc<Inner>) -> axum::Router) -> Self {
        let inner = Arc::new(Inner {
            repos: Mutex::new(repos),
            page,
            log: Log::default(),
            base: Mutex::new(String::new()),
            npm_api,
            canned: Mutex::new(Vec::new()),
        });
        let url = serve(router(inner.clone())).await;
        *inner.base.lock().unwrap() = url.clone();
        Self { url, inner }
    }

    pub async fn nexus(repos: Vec<Repo>, page: usize) -> Self {
        Self::start(repos, page, true, |i| axum::Router::new().fallback(nexus).with_state(i)).await
    }

    pub async fn artifactory(repos: Vec<Repo>, page: usize, npm_api: bool) -> Self {
        Self::start(repos, page, npm_api, |i| axum::Router::new().fallback(artifactory).with_state(i)).await
    }
}

fn escape(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn prefix(name: &str) -> String {
    let n = name.to_lowercase();
    let c: Vec<char> = n.chars().collect();
    match c.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", c[0]),
        _ => format!("{}{}/{}{}/{n}", c[0], c[1], c[2], c[3]),
    }
}

fn decode(p: &str) -> String {
    p.replace("%2f", "/").replace("%2F", "/").replace("%40", "@").replace("%21", "!")
}

/// Serves a repository's own endpoints under `base` (npm, cargo index and
/// downloads, GOPROXY, raw files); `rest` is the path below it.
fn serve_repo(repo: &Repo, base: &str, rest: &str, cargo_dl: &str) -> Response {
    match &repo.content {
        Content::Npm(pkgs) => {
            if let Some((name, file)) = rest.split_once("/-/") {
                return match pkgs.iter().find(|p| p.name == name).and_then(|p| tarball_of(p, file)) {
                    Some(b) => b.into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                };
            }
            match pkgs.iter().find(|p| p.name == rest) {
                Some(p) => Json(packument(base, p)).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        Content::Cargo { crates, index } => {
            if rest == "config.json" && *index {
                return Json(json!({ "dl": cargo_dl })).into_response();
            }
            if let Some(dl) = rest.strip_suffix("/download") {
                let mut parts = dl.rsplitn(3, '/');
                let (v, n) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                return match crates.iter().find(|c| c.name == n && c.version == v) {
                    Some(c) => c.bytes.clone().into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                };
            }
            if let Some(file) = rest.strip_prefix("crates/").and_then(|r| r.split_once('/')).map(|(_, f)| f) {
                if let Some(c) = crates.iter().find(|c| format!("{}-{}.crate", c.name, c.version) == file) {
                    return c.bytes.clone().into_response();
                }
            }
            if *index {
                let lines: Vec<String> =
                    crates.iter().filter(|c| prefix(&c.name) == rest).map(|c| c.line.to_string()).collect();
                if !lines.is_empty() {
                    return (lines.join("\n") + "\n").into_response();
                }
            }
            StatusCode::NOT_FOUND.into_response()
        }
        Content::Go(mods) => {
            let found = mods.iter().find(|m| format!("{}/@v/{}.zip", escape(&m.module), escape(&m.version)) == rest);
            match found {
                Some(m) => m.zip.clone().into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        Content::Raw(files) => match files.iter().find(|(p, _)| p == rest) {
            Some((_, b)) => b.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

fn nexus_components(repo: &Repo) -> Vec<Value> {
    let mut out = Vec::new();
    match &repo.content {
        Content::Npm(pkgs) => {
            for p in pkgs {
                let (group, name) = match p.name.strip_prefix('@').and_then(|s| s.split_once('/')) {
                    Some((g, n)) => (Some(g.to_string()), n.to_string()),
                    None => (None, p.name.clone()),
                };
                for v in &p.versions {
                    out.push(json!({ "id": format!("{}-{}", p.name, v.version), "repository": repo.name, "format": "npm",
                        "group": group, "name": name, "version": v.version,
                        "assets": [{ "path": format!("{}/-/{name}-{}.tgz", p.name, v.version), "checksum": { "sha1": sha1_hex(&v.tarball) } }] }));
                }
            }
        }
        Content::Cargo { crates, .. } => {
            for c in crates {
                out.push(json!({ "id": format!("{}-{}", c.name, c.version), "repository": repo.name, "format": "cargo",
                    "name": c.name, "version": c.version,
                    "assets": [{ "path": format!("crates/{}/{}-{}.crate", c.name, c.name, c.version), "fileSize": c.bytes.len(),
                        "checksum": { "sha256": sha256_hex(&c.bytes), "sha1": sha1_hex(&c.bytes) } }] }));
            }
        }
        Content::Go(mods) => {
            for m in mods {
                out.push(json!({ "id": format!("{}-{}", m.module, m.version), "repository": repo.name, "format": "go",
                    "name": m.module, "version": m.version,
                    "assets": [{ "path": format!("{}/@v/{}.zip", escape(&m.module), escape(&m.version)),
                        "checksum": { "sha256": sha256_hex(&m.zip), "sha1": sha1_hex(&m.zip) } }] }));
            }
        }
        Content::Raw(files) => {
            for (p, b) in files {
                out.push(json!({ "id": p, "repository": repo.name, "format": "raw", "name": p, "version": "",
                    "assets": [{ "path": p, "checksum": { "sha1": sha1_hex(b) } }] }));
            }
        }
    }
    out
}

async fn nexus(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let path = decode(req.uri().path());
    let base = s.base.lock().unwrap().clone();
    let repos = s.repos.lock().unwrap().clone();
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned().collect();
    let q = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    if let Some(r) = canned(&s, &path) {
        return r;
    }
    match path.as_str() {
        "/service/rest/v1/status" => {
            return ([("server", "Nexus/3.70.1-02 (OSS)")], Json(json!({}))).into_response();
        }
        "/service/rest/v1/repositories" => {
            let list: Vec<Value> = repos
                .iter()
                .map(|r| json!({ "name": r.name, "format": r.format(), "type": r.kind, "url": format!("{base}/repository/{}", r.name) }))
                .collect();
            return Json(list).into_response();
        }
        "/service/rest/v1/components" => {
            let Some(repo) = repos.iter().find(|r| Some(&r.name) == q("repository").as_ref()) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let all = nexus_components(repo);
            let from: usize = q("continuationToken").and_then(|t| t.parse().ok()).unwrap_or(0);
            let page: Vec<Value> = all.iter().skip(from).take(s.page).cloned().collect();
            let next = (from + s.page < all.len()).then(|| (from + s.page).to_string());
            return Json(json!({ "items": page, "continuationToken": next })).into_response();
        }
        _ => {}
    }
    let Some(rest) = path.strip_prefix("/repository/") else { return StatusCode::NOT_FOUND.into_response() };
    let Some((name, rest)) = rest.split_once('/') else { return StatusCode::NOT_FOUND.into_response() };
    let Some(repo) = repos.iter().find(|r| r.name == name) else { return StatusCode::NOT_FOUND.into_response() };
    let repo_base = format!("{base}/repository/{name}");
    serve_repo(repo, &repo_base, rest, &format!("{repo_base}/crates/{{crate}}/{{version}}/download"))
}

fn aql_rows(repo: &Repo) -> Vec<Value> {
    let row = |path: String, name: String, bytes: &[u8]| {
        json!({ "repo": repo.name, "path": path, "name": name, "actual_sha1": sha1_hex(bytes), "sha256": sha256_hex(bytes),
            "size": bytes.len(), "modified": "2026-01-01T00:00:00.000Z" })
    };
    let mut out = Vec::new();
    match &repo.content {
        Content::Npm(pkgs) => {
            for p in pkgs {
                let short = p.name.split_once('/').map_or(p.name.as_str(), |(_, n)| n);
                for v in &p.versions {
                    out.push(row(format!("{}/-", p.name), format!("{short}-{}.tgz", v.version), &v.tarball));
                }
                out.push(row(p.name.clone(), "package.json".into(), b"{}"));
            }
        }
        Content::Cargo { crates, .. } => {
            for c in crates {
                out.push(row(format!("crates/{}", c.name), format!("{}-{}.crate", c.name, c.version), &c.bytes));
            }
        }
        Content::Go(mods) => {
            for m in mods {
                let dir = format!("{}/@v", escape(&m.module));
                out.push(row(dir.clone(), format!("{}.zip", escape(&m.version)), &m.zip));
                out.push(row(dir, format!("{}.mod", escape(&m.version)), b"module x\n"));
            }
        }
        Content::Raw(files) => {
            for (p, b) in files {
                let (dir, file) = p.rsplit_once('/').unwrap_or(("", p));
                out.push(row(dir.to_string(), file.to_string(), b));
            }
        }
    }
    out.sort_by(|a, b| (a["path"].as_str(), a["name"].as_str()).cmp(&(b["path"].as_str(), b["name"].as_str())));
    out
}

fn arti_type(kind: &str) -> &'static str {
    match kind {
        "proxy" => "REMOTE",
        "group" => "VIRTUAL",
        _ => "LOCAL",
    }
}

async fn artifactory(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let method = req.method().clone();
    let path = decode(req.uri().path());
    let base = s.base.lock().unwrap().clone();
    let repos = s.repos.lock().unwrap().clone();
    if let Some(r) = canned(&s, &path) {
        return r;
    }
    match path.as_str() {
        "/api/system/version" => return Json(json!({ "version": "7.104.5" })).into_response(),
        "/api/repositories" => {
            let list: Vec<Value> = repos
                .iter()
                .map(|r| {
                    let pt = match r.format() {
                        "raw" => "generic",
                        f => f,
                    };
                    json!({ "key": r.name, "type": arti_type(&r.kind), "packageType": pt })
                })
                .collect();
            return Json(list).into_response();
        }
        "/api/search/aql" if method == axum::http::Method::POST => {
            let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body).to_string();
            let grab = |pre: &str, post: &str| -> Option<String> {
                let start = body.find(pre)? + pre.len();
                let end = body[start..].find(post)? + start;
                Some(body[start..end].to_string())
            };
            let repo_name = grab(r#""$eq":""#, "\"").unwrap_or_default();
            let offset: usize = grab(".offset(", ")").and_then(|o| o.parse().ok()).unwrap_or(0);
            let limit: usize = grab(".limit(", ")").and_then(|o| o.parse().ok()).unwrap_or(1000);
            let Some(repo) = repos.iter().find(|r| r.name == repo_name) else {
                return Json(json!({ "results": [] })).into_response();
            };
            let rows: Vec<Value> = aql_rows(repo).into_iter().skip(offset).take(limit).collect();
            return Json(json!({ "results": rows, "range": { "start_pos": offset, "end_pos": offset + rows.len(), "limit": limit } }))
                .into_response();
        }
        _ => {}
    }
    for (api, kind) in [("/api/npm/", "npm"), ("/api/cargo/", "cargo"), ("/api/go/", "go")] {
        let Some(rest) = path.strip_prefix(api) else { continue };
        let Some((name, rest)) = rest.split_once('/') else { return StatusCode::NOT_FOUND.into_response() };
        let Some(repo) = repos.iter().find(|r| r.name == name && r.format() == kind) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if kind == "npm" && !s.npm_api && !rest.contains("/-/") {
            return StatusCode::NOT_FOUND.into_response();
        }
        let repo_base = format!("{base}{api}{name}");
        let rest = if kind == "cargo" { rest.strip_prefix("index/").unwrap_or(rest) } else { rest };
        return serve_repo(repo, &repo_base, rest, &format!("{base}/api/cargo/{name}/v1/crates"));
    }
    let Some((name, rest)) = path.trim_start_matches('/').split_once('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match repos.iter().find(|r| r.name == name) {
        Some(repo) => serve_repo(repo, &format!("{base}/{name}"), rest, ""),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
