//! The MCP outbound ports over HTTP: an upstream registry's catalog pages.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::ports::mcp_feed::{FeedError, FeedPage, FeedQuery, RegistryFeed};

const PAGE_BYTES: usize = 16 << 20;
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HttpRegistryFeed {
    client: reqwest::Client,
}

impl HttpRegistryFeed {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(PAGE_TIMEOUT)
            .redirect(crate::proxy::redirect_policy())
            .user_agent(concat!("opencargo/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }
}

/// `{base}/v0.1/servers`, the base's own path kept.
pub fn servers_url(base: &url::Url, query: &FeedQuery) -> Result<url::Url, FeedError> {
    let mut url = base.clone();
    let path = format!("{}/v0.1/servers", base.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("limit", &query.limit.clamp(1, 100).to_string());
        if let Some(cursor) = &query.cursor {
            pairs.append_pair("cursor", cursor);
        }
        if let Some(since) = query.updated_since {
            pairs.append_pair("updated_since", &since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
        pairs.append_pair("include_deleted", if query.include_deleted { "true" } else { "false" });
    }
    Ok(url)
}

#[async_trait]
impl RegistryFeed for HttpRegistryFeed {
    async fn page(&self, base: &url::Url, query: &FeedQuery) -> Result<FeedPage, FeedError> {
        let url = servers_url(base, query)?;
        let resp = self
            .client
            .get(url.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| FeedError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(FeedError::Status(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n as usize > PAGE_BYTES) {
            return Err(FeedError::Invalid("page too large".into()));
        }
        let bytes = resp.bytes().await.map_err(|e| FeedError::Transport(e.to_string()))?;
        if bytes.len() > PAGE_BYTES {
            return Err(FeedError::Invalid("page too large".into()));
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| FeedError::Invalid(e.to_string()))?;
        let servers = body
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| FeedError::Invalid("no servers array".into()))?;
        let next_cursor = body
            .get("metadata")
            .and_then(|m| m.get("nextCursor"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        Ok(FeedPage { servers, next_cursor })
    }
}

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";
const KNOWN_VERSIONS: &[&str] = &["2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"];
const PROBE_BUDGET: Duration = Duration::from_secs(20);
const MAX_FRAMES: usize = 256;
const MAX_ANSWER_BYTES: usize = 4 << 20;
const MAX_TOOL_PAGES: usize = 10;
const HEADER_MISMATCH: i64 = -32020;

/// `tools/list` over streamable HTTP: the stateless modern exchange first,
/// the initialization-era session when the server answers in that era.
pub struct HttpToolProbe {
    budget: Duration,
}

impl HttpToolProbe {
    pub fn new() -> Self {
        Self { budget: PROBE_BUDGET }
    }
}

impl Default for HttpToolProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::ports::mcp_feed::ToolProbe for HttpToolProbe {
    async fn tools(
        &self,
        url: &str,
        options: &crate::ports::mcp_feed::ProbeOptions,
    ) -> Result<crate::ports::mcp_feed::ProbeAnswer, String> {
        match tokio::time::timeout(self.budget, probe(url, options)).await {
            Ok(answer) => answer,
            Err(_) => Err(format!("no answer within {}s", self.budget.as_secs())),
        }
    }
}

async fn probe(url: &str, options: &crate::ports::mcp_feed::ProbeOptions) -> Result<crate::ports::mcp_feed::ProbeAnswer, String> {
    crate::proxy::validate_upstream_url(url).map_err(|e| e.to_string())?;
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    if !options.allow_private {
        crate::proxy::refuse_blocked_host(&parsed).await.map_err(|e| e.to_string())?;
    }
    let client = reqwest::Client::builder()
        .redirect(crate::proxy::redirect_policy())
        .user_agent(concat!("opencargo/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let session = Session {
        client,
        url: parsed,
        auth: options.auth_header.clone(),
    };
    match session.modern(MODERN).await? {
        Modern::Answer(tools) => Ok(answer(tools, MODERN)),
        Modern::Retry(version) if version.as_str() >= MODERN => match session.modern(&version).await? {
            Modern::Answer(tools) => Ok(answer(tools, &version)),
            _ => Err(format!("the server refused {version}, which it advertised")),
        },
        Modern::Retry(version) => session.legacy(&version).await,
        Modern::Legacy => session.legacy(LEGACY).await,
    }
}

fn answer(tools: Vec<Value>, version: &str) -> crate::ports::mcp_feed::ProbeAnswer {
    crate::ports::mcp_feed::ProbeAnswer {
        tools,
        protocol_version: version.to_string(),
    }
}

enum Modern {
    Answer(Vec<Value>),
    Retry(String),
    Legacy,
}

struct Session {
    client: reqwest::Client,
    url: reqwest::Url,
    auth: Option<(String, String)>,
}

struct Reply {
    status: u16,
    session: Option<String>,
    message: Option<Value>,
}

fn client_meta(version: &str) -> Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {"name": "opencargo", "version": env!("CARGO_PKG_VERSION")},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

impl Session {
    async fn post(&self, body: &Value, headers: &[(&str, String)]) -> Result<Reply, String> {
        let mut req = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .json(body);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if let Some((k, v)) = &self.auth {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status().as_u16();
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let id = body.get("id").cloned();
        let message = match id {
            Some(id) => read_message(resp, &id).await?,
            None => None,
        };
        Ok(Reply { status, session, message })
    }

    /// One stateless `tools/list`, following `nextCursor`.
    async fn modern(&self, version: &str) -> Result<Modern, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        for page in 0..MAX_TOOL_PAGES {
            let mut params = serde_json::json!({"_meta": client_meta(version)});
            if let Some(c) = &cursor {
                params["cursor"] = c.clone();
            }
            let body = serde_json::json!({"jsonrpc": "2.0", "id": page + 1, "method": "tools/list", "params": params});
            let headers = [("MCP-Protocol-Version", version.to_string()), ("Mcp-Method", "tools/list".to_string())];
            let reply = self.post(&body, &headers).await?;
            if let Some(error) = reply.message.as_ref().and_then(|m| m.get("error")) {
                return recognised(error).map_err(|why| why.to_string()).and_then(|r| match r {
                    Some(version) => Ok(Modern::Retry(version)),
                    None if matches!(reply.status, 400 | 404 | 405) => Ok(Modern::Legacy),
                    None => Err(format!("tools/list failed: {error}")),
                });
            }
            if matches!(reply.status, 400 | 404 | 405) {
                return Ok(Modern::Legacy);
            }
            let result = ok_result(&reply)?;
            append_tools(&mut tools, result)?;
            match result.get("nextCursor").filter(|c| !c.is_null()) {
                Some(next) => cursor = Some(next.clone()),
                None => return Ok(Modern::Answer(tools)),
            }
        }
        Ok(Modern::Answer(tools))
    }

    async fn initialize(&self, version: &str) -> Result<(Option<String>, String), String> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": version, "capabilities": {},
            "clientInfo": {"name": "opencargo", "version": env!("CARGO_PKG_VERSION")}}});
        let reply = self.post(&body, &[]).await?;
        let result = ok_result(&reply)?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(version)
            .to_string();
        let mut headers = vec![("MCP-Protocol-Version", negotiated.clone())];
        if let Some(s) = &reply.session {
            headers.push(("Mcp-Session-Id", s.clone()));
        }
        let note = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let ack = self.post(&note, &headers).await?;
        if !(200..300).contains(&ack.status) {
            return Err(format!("notifications/initialized answered {}", ack.status));
        }
        Ok((reply.session, negotiated))
    }

    /// The initialization-era session: initialize, the initialized
    /// notification, then `tools/list` carrying the session and the
    /// negotiated version; an expired session is re-initialized once.
    async fn legacy(&self, version: &str) -> Result<crate::ports::mcp_feed::ProbeAnswer, String> {
        let (mut session, mut negotiated) = self.initialize(version).await?;
        let mut renewed = false;
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        let mut page = 0;
        while page < MAX_TOOL_PAGES {
            let mut params = serde_json::json!({});
            if let Some(c) = &cursor {
                params["cursor"] = c.clone();
            }
            let body = serde_json::json!({"jsonrpc": "2.0", "id": page + 2, "method": "tools/list", "params": params});
            let mut headers = vec![("MCP-Protocol-Version", negotiated.clone())];
            if let Some(s) = &session {
                headers.push(("Mcp-Session-Id", s.clone()));
            }
            let reply = self.post(&body, &headers).await?;
            if reply.status == 404 && session.is_some() && !renewed {
                renewed = true;
                (session, negotiated) = self.initialize(version).await?;
                continue;
            }
            let result = ok_result(&reply)?;
            append_tools(&mut tools, result)?;
            match result.get("nextCursor").filter(|c| !c.is_null()) {
                Some(next) => cursor = Some(next.clone()),
                None => break,
            }
            page += 1;
        }
        Ok(answer(tools, &negotiated))
    }
}

fn ok_result(reply: &Reply) -> Result<&Value, String> {
    if !(200..300).contains(&reply.status) {
        return Err(format!("the server answered {}", reply.status));
    }
    let message = reply.message.as_ref().ok_or("the server answered no JSON-RPC response")?;
    if let Some(error) = message.get("error") {
        return Err(format!("the server answered an error: {error}"));
    }
    message.get("result").ok_or_else(|| "a JSON-RPC response with no result".to_string())
}

fn append_tools(tools: &mut Vec<Value>, result: &Value) -> Result<(), String> {
    let page = result.get("tools").and_then(Value::as_array).ok_or("a result with no tools array")?;
    tools.extend(page.iter().cloned());
    Ok(())
}

/// A modern error answered rather than abandoned: an unsupported version
/// is retried at the highest advertised one we know; a header mismatch is
/// our own malformed request; a capability we cannot honestly claim ends
/// the run. `Ok(None)` is an error of no recognised shape.
fn recognised(error: &Value) -> Result<Option<String>, String> {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or_default();
    let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
    if code == HEADER_MISMATCH {
        return Err(format!("header mismatch: {message}"));
    }
    let data = error.get("data");
    if let Some(supported) = data.and_then(|d| d.get("supported")).and_then(Value::as_array) {
        let best = supported
            .iter()
            .filter_map(Value::as_str)
            .filter(|v| KNOWN_VERSIONS.contains(v))
            .max()
            .ok_or_else(|| format!("no supported protocol version in common: {supported:?}"))?;
        return Ok(Some(best.to_string()));
    }
    let capability = data
        .and_then(|d| d.get("requiredCapabilities").or_else(|| d.get("capability")))
        .map(|c| c.to_string());
    if capability.is_some() || message.to_ascii_lowercase().contains("capabilit") {
        return Err(format!("missing client capability: {}", capability.unwrap_or_else(|| message.to_string())));
    }
    Ok(None)
}

/// The JSON-RPC response carrying `id`, from a JSON body or from an event
/// stream read frame by frame and dropped at the response: a server may
/// hold the stream open after it.
async fn read_message(mut resp: reqwest::Response, id: &Value) -> Result<Option<Value>, String> {
    let sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !sse {
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_ANSWER_BYTES {
                return Err("answer too large".into());
            }
        }
        return Ok(serde_json::from_slice(&bytes).ok());
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut data = String::new();
    let (mut total, mut frames) = (0usize, 0usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        total += chunk.len();
        if total > MAX_ANSWER_BYTES {
            return Err("event stream too large".into());
        }
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !data.is_empty() {
                    frames += 1;
                    if frames > MAX_FRAMES {
                        return Err("too many event frames".into());
                    }
                    if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                        if msg.get("id") == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some()) {
                            return Ok(Some(msg));
                        }
                    }
                    data.clear();
                }
            } else if let Some(payload) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(payload.trim_start());
            }
        }
    }
    Ok(serde_json::from_str::<Value>(&data).ok().filter(|m| m.get("id") == Some(id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_over_100_is_clamped_before_the_request_and_the_flag_is_explicit() {
        let base = url::Url::parse("https://registry.example/prefix/").unwrap();
        let url = servers_url(
            &base,
            &FeedQuery {
                cursor: Some("a/b:1.0.0".into()),
                updated_since: None,
                limit: 500,
                include_deleted: true,
            },
        )
        .unwrap();
        assert_eq!(url.path(), "/prefix/v0.1/servers");
        let pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
        assert!(pairs.contains(&("limit".into(), "100".into())));
        assert!(pairs.contains(&("cursor".into(), "a/b:1.0.0".into())));
        assert!(pairs.contains(&("include_deleted".into(), "true".into())));
    }
}
