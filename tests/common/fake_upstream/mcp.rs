//! A fake MCP registry: `name:version` cursor order, `updated_since` as a
//! pure filter over deliberately non-monotonic `updatedAt`, a 422 above
//! `limit=100`, a 503 switch, and a hook that edits a record sorting
//! behind the cursor while a client is paging.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub const OFFICIAL: &str = "io.modelcontextprotocol.registry/official";

#[derive(Clone)]
pub struct Entry {
    pub server: Value,
    pub status: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Default)]
pub struct Registry {
    pub records: BTreeMap<(String, String), Entry>,
    pub unavailable: bool,
    /// Applied once, after the first page with a cursor is served.
    pub behind_cursor: Option<(String, String, String, DateTime<Utc>)>,
    pub queries: Vec<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct FakeRegistry {
    pub base_url: String,
    pub state: Arc<Mutex<Registry>>,
}

impl FakeRegistry {
    pub fn put(&self, server: Value, status: &str, updated_at: DateTime<Utc>) {
        let key = (
            server["name"].as_str().unwrap().to_string(),
            server["version"].as_str().unwrap().to_string(),
        );
        self.state.lock().unwrap().records.insert(
            key,
            Entry {
                server,
                status: status.to_string(),
                updated_at,
            },
        );
    }

    pub fn queries(&self) -> Vec<HashMap<String, String>> {
        self.state.lock().unwrap().queries.clone()
    }

    pub fn set_unavailable(&self, on: bool) {
        self.state.lock().unwrap().unavailable = on;
    }
}

fn envelope(e: &Entry) -> Value {
    json!({
        "server": e.server,
        "_meta": {OFFICIAL: {
            "status": e.status,
            "statusChangedAt": e.updated_at.to_rfc3339(),
            "publishedAt": "2026-01-01T00:00:00Z",
            "updatedAt": e.updated_at.to_rfc3339(),
            "isLatest": true,
        }},
    })
}

async fn servers(State(state): State<Arc<Mutex<Registry>>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut reg = state.lock().unwrap();
    reg.queries.push(q.clone());
    if reg.unavailable {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(30);
    if limit > 100 {
        return (StatusCode::UNPROCESSABLE_ENTITY, "validation failed: expected number <= 100").into_response();
    }
    let since: Option<DateTime<Utc>> = q
        .get("updated_since")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let include_deleted = q.get("include_deleted").map(|v| v == "true").unwrap_or(since.is_some());
    let after = q.get("cursor").and_then(|c| c.split_once(':')).map(|(n, v)| (n.to_string(), v.to_string()));
    let rows: Vec<Value> = reg
        .records
        .iter()
        .filter(|(k, _)| after.as_ref().is_none_or(|a| *k > a))
        .filter(|(_, e)| since.is_none_or(|s| e.updated_at >= s))
        .filter(|(_, e)| include_deleted || e.status != "deleted")
        .take(limit + 1)
        .map(|(_, e)| envelope(e))
        .collect();
    let more = rows.len() > limit;
    let page: Vec<Value> = rows.into_iter().take(limit).collect();
    let mut metadata = json!({"count": page.len()});
    if more {
        let last = &page[page.len() - 1]["server"];
        metadata["nextCursor"] = json!(format!("{}:{}", last["name"].as_str().unwrap(), last["version"].as_str().unwrap()));
        if let Some((name, version, description, at)) = reg.behind_cursor.take() {
            if let Some(e) = reg.records.get_mut(&(name, version)) {
                e.server["description"] = json!(description);
                e.updated_at = at;
            }
        }
    }
    Json(json!({"servers": page, "metadata": metadata})).into_response()
}

pub async fn start() -> FakeRegistry {
    let state = Arc::new(Mutex::new(Registry::default()));
    let app = Router::new()
        .route("/v0.1/servers", get(servers))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeRegistry { base_url, state }
}

pub fn server(name: &str, version: &str) -> Value {
    json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": name,
        "description": format!("{name} does things"),
        "version": version,
        "remotes": [{"type": "streamable-http", "url": format!("https://{}.example/mcp", name.replace('/', "-"))}],
    })
}
