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

/// How the fake MCP server speaks.
#[derive(Clone, Debug, PartialEq)]
pub enum Speaks {
    /// 2026-07-28: stateless, `Mcp-Method` and the namespaced `_meta`
    /// checked against the headers.
    Modern,
    /// The initialization era: a session minted on `initialize`, required
    /// with `MCP-Protocol-Version` afterwards, after the initialized
    /// notification; `expire_once` answers the first `tools/list` 404.
    Legacy { expire_once: bool },
    /// Modern, answered as an event stream that stays open afterwards.
    StreamOpen,
    /// A modern request is refused advertising `2025-11-25`, which then
    /// speaks the initialization era.
    Unsupported,
    HeaderMismatch,
    Redirect(String),
}

#[derive(Default)]
pub struct McpState {
    pub tools: Vec<Value>,
    pub sessions: std::collections::HashSet<String>,
    pub initialized: std::collections::HashSet<String>,
    pub expired: bool,
    /// Every request: its method and the `_meta` keys it carried.
    pub log: Vec<(String, Vec<String>)>,
}

#[derive(Clone)]
pub struct FakeMcp {
    pub url: String,
    pub speaks: Speaks,
    pub state: Arc<Mutex<McpState>>,
}

impl FakeMcp {
    pub fn methods(&self) -> Vec<String> {
        self.state.lock().unwrap().log.iter().map(|(m, _)| m.clone()).collect()
    }
}

fn rpc_error(status: StatusCode, code: i64, message: &str, data: Value) -> Response {
    (status, Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message, "data": data}}))).into_response()
}

fn header<'a>(h: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn tools_result(id: &Value, tools: &[Value]) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})
}

/// `None`: the headers do not match the request, which a modern server
/// refuses with a header mismatch.
fn modern(state: &McpState, headers: &axum::http::HeaderMap, body: &Value) -> Option<Value> {
    let method = body["method"].as_str().unwrap_or_default();
    let version = body.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion").and_then(Value::as_str);
    if header(headers, "mcp-method") != Some(method) || header(headers, "mcp-protocol-version") != version || version.is_none() {
        return None;
    }
    Some(tools_result(&body["id"], &state.tools))
}

fn mismatch() -> Response {
    rpc_error(StatusCode::BAD_REQUEST, -32020, "Mcp-Method header is required from 2026-07-28", json!({}))
}

fn legacy(state: &mut McpState, headers: &axum::http::HeaderMap, body: &Value, expire_once: bool) -> Response {
    let method = body["method"].as_str().unwrap_or_default().to_string();
    if method == "initialize" {
        let session = format!("s{}", state.sessions.len() + 1);
        state.sessions.insert(session.clone());
        let result = json!({"jsonrpc": "2.0", "id": body["id"], "result": {
            "protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}});
        return ([("mcp-session-id", session)], Json(result)).into_response();
    }
    let session = header(headers, "mcp-session-id").map(str::to_string);
    let Some(session) = session.filter(|s| state.sessions.contains(s)) else {
        let status = if header(headers, "mcp-session-id").is_some() { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
        return rpc_error(status, -32000, "Bad Request: No valid session ID provided", Value::Null);
    };
    if header(headers, "mcp-protocol-version").is_none() {
        return rpc_error(StatusCode::BAD_REQUEST, -32000, "Bad Request: missing MCP-Protocol-Version", Value::Null);
    }
    match method.as_str() {
        "notifications/initialized" => {
            state.initialized.insert(session);
            StatusCode::ACCEPTED.into_response()
        }
        "tools/list" if !state.initialized.contains(&session) => {
            rpc_error(StatusCode::BAD_REQUEST, -32000, "Bad Request: not initialized", Value::Null)
        }
        "tools/list" if expire_once && !state.expired => {
            state.expired = true;
            state.sessions.remove(&session);
            StatusCode::NOT_FOUND.into_response()
        }
        "tools/list" => Json(tools_result(&body["id"], &state.tools)).into_response(),
        _ => rpc_error(StatusCode::BAD_REQUEST, -32601, "method not found", Value::Null),
    }
}

fn open_stream(response: Value) -> Response {
    let frames: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
        Ok(": keep-alive\n\n".into()),
        Ok(format!("data: {}\n\n", json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}})).into()),
        Ok(format!("data: {}\n\n", json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}})).into()),
        Ok(format!("data: {response}\n\n").into()),
    ];
    let stream = futures_util::StreamExt::chain(futures_util::stream::iter(frames), futures_util::stream::pending());
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn mcp_endpoint(
    State((speaks, state)): State<(Speaks, Arc<Mutex<McpState>>)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut st = state.lock().unwrap();
    let keys = body
        .pointer("/params/_meta")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    st.log.push((body["method"].as_str().unwrap_or_default().to_string(), keys));
    let is_modern = header(&headers, "mcp-method").is_some() && header(&headers, "mcp-protocol-version") == Some("2026-07-28");
    match &speaks {
        Speaks::Modern => modern(&st, &headers, &body).map_or_else(mismatch, |r| Json(r).into_response()),
        Speaks::StreamOpen => modern(&st, &headers, &body).map_or_else(mismatch, open_stream),
        Speaks::HeaderMismatch => rpc_error(StatusCode::BAD_REQUEST, -32020, "header mismatch", json!({})),
        Speaks::Unsupported if is_modern => rpc_error(
            StatusCode::BAD_REQUEST,
            -32602,
            "Unsupported protocol version",
            json!({"supported": ["2025-11-25"]}),
        ),
        Speaks::Unsupported => legacy(&mut st, &headers, &body, false),
        Speaks::Legacy { expire_once } => legacy(&mut st, &headers, &body, *expire_once),
        Speaks::Redirect(to) => (StatusCode::TEMPORARY_REDIRECT, [(axum::http::header::LOCATION, to.clone())]).into_response(),
    }
}

pub async fn start_mcp(speaks: Speaks, tools: Vec<Value>) -> FakeMcp {
    let state = Arc::new(Mutex::new(McpState {
        tools,
        ..McpState::default()
    }));
    let app = Router::new()
        .route("/mcp", axum::routing::post(mcp_endpoint))
        .with_state((speaks.clone(), state.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeMcp { url, speaks, state }
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
