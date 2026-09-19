//! Writes into the registry of record: a hosted repository takes a
//! `server.json` and mints the official block itself, and any repository
//! takes the `tools/list` a runner attests for a stdio server.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::ingest::{detail_of, observed_surface, record_write};
use super::schema::{parse, SchemaId, OFFICIAL_META};
use super::surface::Tool;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::mcp::SurfaceSource;
use crate::server::AppState;

const MAX_RECORD_BYTES: usize = 1 << 20;
const MAX_ATTESTED_BYTES: usize = 1 << 20;
const MAX_ATTESTED_TOOLS: usize = 512;

fn caller(request: &axum::http::Request<axum::body::Body>) -> AppResult<AuthUser> {
    request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

async fn mcp_repo(state: &AppState, name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Mcp)?;
    Ok(repo)
}

async fn json_body(request: axum::http::Request<axum::body::Body>, limit: usize) -> AppResult<Value> {
    let bytes = axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// POST /{repo}/v0.1/publish -- a `server.json` into a hosted repository.
/// The official block is ours to mint here: status active, `publishedAt`
/// kept on a republish, and `isLatest` moved by publication order.
pub async fn publish(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let auth = caller(&request)?;
    let repo = mcp_repo(&state, &repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth).await?;
    let mut record = json_body(request, MAX_RECORD_BYTES).await?;
    if let Some(meta) = record.get_mut("_meta").and_then(Value::as_object_mut) {
        meta.remove(super::schema::MIRROR_META);
    }
    let (detail, schema) = parse(&record)?;
    if let SchemaId::Unknown(url) = schema {
        return Err(AppError::BadRequest(format!("unknown server.json schema: {url}")));
    }
    super::ingest::validate(&detail)?;
    let now = state.clock.now();
    let previous = state.mcp.version(repo.id, repo.id, &detail.name, Some(&detail.version)).await?;
    let published_at = previous
        .as_ref()
        .and_then(|row| serde_json::from_str::<Value>(&row.envelope_json).ok())
        .and_then(|e| e.pointer(&format!("/_meta/{}/publishedAt", OFFICIAL_META.replace('/', "~1"))).cloned())
        .unwrap_or_else(|| json!(now.to_rfc3339()));
    let envelope = json!({
        "server": record,
        "_meta": {OFFICIAL_META: {
            "status": "active",
            "statusChangedAt": now.to_rfc3339(),
            "publishedAt": published_at,
            "updatedAt": now.to_rfc3339(),
            "isLatest": true,
        }},
    });
    let mut write = record_write(repo.id, &envelope, true, now)?;
    write.take_latest = true;
    let done = state.mcp.upsert_record(&write).await?;
    crate::api::record_audit(&state, &auth, "mcp.publish", Some(&format!("{}@{}", detail.name, detail.version))).await;
    let status = if previous.is_some() { StatusCode::OK } else { StatusCode::CREATED };
    let row = state
        .mcp
        .version_by_id(done.version_id, repo.id)
        .await?
        .ok_or_else(|| AppError::Internal("published row vanished".into()))?;
    Ok((status, Json(serde_json::from_str(&row.envelope_json)?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attestation {
    pub name: String,
    pub version: String,
    pub tools: Vec<Value>,
    pub runner: Option<String>,
    pub protocol_version: Option<String>,
}

/// POST /{repo}/v0.1/surfaces -- the tools a runner saw a stdio server
/// answer: the one honest way to fingerprint what opencargo never runs.
pub async fn attest(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Json<Value>> {
    let auth = caller(&request)?;
    let repo = mcp_repo(&state, &repo_name).await?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth).await?;
    let body: Attestation = serde_json::from_value(json_body(request, MAX_ATTESTED_BYTES).await?)?;
    if body.tools.len() > MAX_ATTESTED_TOOLS {
        return Err(AppError::BadRequest(format!("at most {MAX_ATTESTED_TOOLS} tools")));
    }
    let tools: Vec<Tool> = body
        .tools
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|e| AppError::BadRequest(format!("invalid tool: {e}")))?;
    let row = state
        .mcp
        .version(repo.id, repo.id, &body.name, Some(&body.version))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("server not found: {}@{}", body.name, body.version)))?;
    let detail = detail_of(&row.envelope_json)?;
    let runner = body.runner.unwrap_or_else(|| auth.username.clone());
    let surface = observed_surface(&detail, SurfaceSource::Attested, "", &tools, Some(runner))?;
    let high = surface.findings.iter().filter(|f| f.high).count();
    let id = state.mcp.record_attested(row.version_id, &surface, state.clock.now()).await?;
    crate::api::record_audit(&state, &auth, "mcp.attest", Some(&format!("{}@{}", body.name, body.version))).await;
    Ok(Json(json!({
        "surfaceId": id,
        "toolsSha256": surface.tools_sha256,
        "protocolVersion": body.protocol_version,
        "findings": {"high": high, "total": surface.findings.len()},
    })))
}
