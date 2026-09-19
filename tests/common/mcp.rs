//! MCP fixtures: records, envelopes, and rows written through the store the
//! server runs on, so a read test needs no upstream.

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{json, Value};

use opencargo::config::{McpConfig, RepositoryConfig, RepositoryFormat, Visibility};
use opencargo::domain::governance::Decision;
use opencargo::ports::mcp::{McpStore, NewApproval};

use super::TestServer;

pub const MCP: RepositoryFormat = RepositoryFormat::Mcp;
pub const OFFICIAL: &str = "io.modelcontextprotocol.registry/official";
pub const MIRROR: &str = "eu.opencargo.registry/mirror";

pub fn mirror(name: &str) -> RepositoryConfig {
    super::proxy(name, MCP, "https://registry.example.invalid")
}

pub fn hosted(name: &str, vis: Visibility) -> RepositoryConfig {
    super::hosted(name, MCP, vis)
}

pub fn mode(mode: &str, allow: &[&str], deny: &[&str]) -> McpConfig {
    McpConfig {
        mode: serde_json::from_value(json!(mode)).unwrap(),
        allowlist: allow.iter().map(|s| s.to_string()).collect(),
        denylist: deny.iter().map(|s| s.to_string()).collect(),
        ..McpConfig::default()
    }
}

pub fn settings(entries: &[(&str, McpConfig)]) -> HashMap<String, McpConfig> {
    entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

/// A record the way the registry serves it.
pub fn record(name: &str, version: &str) -> Value {
    json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": name,
        "description": format!("{name} does things"),
        "version": version,
        "remotes": [{"type": "streamable-http", "url": format!("https://{}.example/mcp", name.replace('/', "-"))}],
    })
}

pub fn envelope(record: Value, latest: bool) -> Value {
    json!({
        "server": record,
        "_meta": {OFFICIAL: {
            "status": "active",
            "statusChangedAt": "2026-04-13T17:32:20.852269Z",
            "publishedAt": "2026-04-13T17:32:20.852269Z",
            "updatedAt": "2026-04-13T17:32:20.852269Z",
            "isLatest": latest,
        }},
    })
}

pub async fn store(server: &TestServer) -> (std::sync::Arc<dyn McpStore>, std::sync::Arc<dyn opencargo::ports::repositories::RepositoryStore>) {
    let stores = opencargo::server::open_stores(&server.tmp.path().join("opencargo.db"))
        .await
        .expect("failed to open the server database");
    (stores.mcp(), stores.repositories())
}

pub async fn repo_id(server: &TestServer, name: &str) -> i64 {
    let (_, repos) = store(server).await;
    repos.by_name(name).await.unwrap().expect("repository").id
}

/// Envelopes written into `repo` as a sync would write them.
pub async fn seed(server: &TestServer, repo: &str, envelopes: &[Value]) {
    let (mcp, repos) = store(server).await;
    let id = repos.by_name(repo).await.unwrap().expect("repository").id;
    for e in envelopes {
        let write = opencargo::registry::mcp::ingest::record_write(id, e, false, Utc::now()).unwrap();
        mcp.upsert_record(&write).await.unwrap();
    }
}

/// `n` versions of distinct names, each its own latest.
pub async fn seed_many(server: &TestServer, repo: &str, names: impl IntoIterator<Item = String>) {
    let envelopes: Vec<Value> = names.into_iter().map(|n| envelope(record(&n, "1.0.0"), true)).collect();
    seed(server, repo, &envelopes).await;
}

/// An approval by `repo` of the version's current record.
pub async fn approve(server: &TestServer, repo: &str, name: &str, version: &str) {
    approve_all(server, repo, &[name.to_string()], version).await;
}

pub async fn approve_all(server: &TestServer, repo: &str, names: &[String], version: &str) {
    let (mcp, repos) = store(server).await;
    let addressed = repos.by_name(repo).await.unwrap().expect("repository").id;
    let mut approvals = Vec::new();
    for name in names {
        let row = find_version(mcp.as_ref(), repos.as_ref(), name, version).await;
        let current = row.current.expect("a current surface");
        approvals.push(NewApproval {
            repository: addressed,
            skill: false,
            name: name.clone(),
            version: version.into(),
            remote_url: current.remote_url.clone(),
            permissions_sha256: current.permissions_sha256.clone(),
            tools_sha256: current.tools_sha256.clone(),
            combined_sha256: current.combined_sha256.clone(),
            surface_id: Some(current.id),
            decision: Decision::Approved,
            decided_by: "admin".into(),
            note: None,
            now: Utc::now(),
        });
    }
    mcp.decide(&approvals).await.unwrap();
}

async fn find_version(
    mcp: &dyn McpStore,
    repos: &dyn opencargo::ports::repositories::RepositoryStore,
    name: &str,
    version: &str,
) -> opencargo::ports::mcp::CatalogRow {
    for repo in repos.all().await.unwrap() {
        if let Some(row) = mcp.version(repo.id, repo.id, name, Some(version)).await.unwrap() {
            return row;
        }
    }
    panic!("no member holds {name}@{version}");
}

pub async fn get(server: &TestServer, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::get(format!("{}{path}", server.base_url)).await.unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Every `name@version` of a list page.
pub fn names(page: &Value) -> Vec<String> {
    page["servers"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|s| format!("{}@{}", s["server"]["name"].as_str().unwrap(), s["server"]["version"].as_str().unwrap()))
        .collect()
}

/// Walk the cursor to the end, every page's rows in order.
pub async fn walk(server: &TestServer, repo: &str, limit: usize, extra: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10_000 {
        let mut path = format!("/{repo}/v0.1/servers?limit={limit}{extra}");
        if let Some(c) = &cursor {
            path.push_str(&format!("&cursor={}", urlencode(c)));
        }
        let (status, page) = get(server, &path).await;
        assert_eq!(status, 200, "{page}");
        out.extend(names(&page));
        match page["metadata"]["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => return out,
        }
    }
    panic!("the cursor never ended");
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
