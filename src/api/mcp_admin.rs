//! The admin surface of MCP governance: allow rules, suppressions, the
//! catalog by review state, one version's evidence, and the decisions.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{actor, require_admin, require_auth};
use crate::app::mcp::decide::DecideVersion;
use crate::auth::middleware::AuthUser;
use crate::domain::governance::{validate_pattern, AllowRule, Decision, Effect};
use crate::domain::{Drift, Format, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::mcp::{CatalogRow, PageQuery, SurfaceSource};
use crate::registry::mcp::catalog::scope;
use crate::registry::mcp::ingest::detail_of;
use crate::registry::mcp::surface::{header_params, Tool};
use crate::server::AppState;

type Request = axum::http::Request<axum::body::Body>;

fn admin(request: &Request) -> AppResult<AuthUser> {
    let caller = require_auth(request)?;
    require_admin(&caller)?;
    Ok(caller)
}

async fn mcp_repo(state: &AppState, name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Mcp)?;
    Ok(repo)
}

async fn body<T: for<'de> Deserialize<'de>>(request: Request) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn review_state(row: &CatalogRow) -> &'static str {
    let approved = row.surface_endpoints > 0 && row.approved_endpoints == row.surface_endpoints && row.worst_drift == Drift::None;
    match row.decision {
        Some(Decision::Blocked) => "blocked",
        _ if approved => "approved",
        _ if row.worst_drift != Drift::None => "drifted",
        _ => "pending",
    }
}

fn row_json(row: &CatalogRow) -> Value {
    json!({
        "name": row.name,
        "version": row.version,
        "status": row.status,
        "isLatest": row.is_latest,
        "hosted": row.hosted,
        "state": review_state(row),
        "drift": row.worst_drift.as_str(),
        "driftedRemote": row.drifted_remote,
        "endpoints": {"approved": row.approved_endpoints, "total": row.surface_endpoints},
        "transports": {"packages": row.package_transports, "remotes": row.remote_transports},
        "toolsSource": row.current.as_ref().map(|c| c.source.as_str()),
        "findings": {"high": row.findings_high, "medium": row.findings_medium},
        "syncedAt": row.synced_at.to_rfc3339(),
    })
}

#[derive(Deserialize)]
pub struct RuleRequest {
    pub pattern: String,
    pub effect: Effect,
}

pub async fn list_rules(State(state): State<AppState>, Path(name): Path<String>, request: Request) -> AppResult<Json<Value>> {
    admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let rules = state.mcp.allow_rules(repo.id).await?;
    Ok(Json(json!(rules
        .iter()
        .map(|r| json!({"id": r.id, "pattern": r.rule.pattern, "effect": r.rule.effect}))
        .collect::<Vec<_>>())))
}

pub async fn add_rule(State(state): State<AppState>, Path(name): Path<String>, request: Request) -> AppResult<(StatusCode, Json<Value>)> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let req: RuleRequest = body(request).await?;
    validate_pattern(&req.pattern)?;
    let rule = AllowRule::new(&req.pattern, req.effect)?;
    let id = state.mcp.add_allow_rule(repo.id, &rule, Some(&caller.username), state.clock.now()).await?;
    crate::api::record_audit(&state, &caller, "mcp.rule.add", Some(&format!("{name}: {} {}", req.effect.as_str(), req.pattern))).await;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

pub async fn delete_rule(State(state): State<AppState>, Path((name, id)): Path<(String, i64)>, request: Request) -> AppResult<StatusCode> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    state.mcp.delete_allow_rule(repo.id, id).await?;
    crate::api::record_audit(&state, &caller, "mcp.rule.delete", Some(&format!("{name}: {id}"))).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SuppressRequest {
    pub pattern: String,
    #[serde(default)]
    pub tool: String,
}

pub async fn list_suppressions(State(state): State<AppState>, Path(name): Path<String>, request: Request) -> AppResult<Json<Value>> {
    admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let rows = state.mcp.suppressions(repo.id).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|s| json!({"id": s.id, "pattern": s.pattern, "tool": s.tool}))
        .collect::<Vec<_>>())))
}

pub async fn add_suppression(State(state): State<AppState>, Path(name): Path<String>, request: Request) -> AppResult<(StatusCode, Json<Value>)> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let req: SuppressRequest = body(request).await?;
    if req.pattern.is_empty() {
        return Err(AppError::BadRequest("a suppression names a pattern".into()));
    }
    let id = state
        .mcp
        .add_suppression(repo.id, &req.pattern, &req.tool, Some(&caller.username), state.clock.now())
        .await?;
    crate::api::record_audit(&state, &caller, "mcp.suppress", Some(&format!("{name}: {}:{}", req.pattern, req.tool))).await;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

pub async fn delete_suppression(State(state): State<AppState>, Path((name, id)): Path<(String, i64)>, request: Request) -> AppResult<StatusCode> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    state.mcp.delete_suppression(repo.id, id, state.clock.now()).await?;
    crate::api::record_audit(&state, &caller, "mcp.unsuppress", Some(&format!("{name}: {id}"))).await;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/v1/mcp/{repo}/servers?state=pending|drifted|blocked|approved|not_observed|all&q=&limit=
pub async fn servers(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> AppResult<Json<Value>> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let (members, _) = scope(&state, &repo, Some(&caller)).await?;
    let wanted = params.get("state").map(String::as_str).unwrap_or("all");
    let limit: usize = params.get("limit").and_then(|l| l.parse().ok()).unwrap_or(200).min(1000);
    let mut out = Vec::new();
    for member in &members {
        let mut after = None;
        while out.len() < limit {
            let rows = state
                .mcp
                .page(&PageQuery {
                    member: member.id,
                    addressed: repo.id,
                    after: after.clone(),
                    limit: 500,
                    include_deleted: true,
                    search: params.get("q").filter(|q| !q.is_empty()).cloned(),
                    ..PageQuery::default()
                })
                .await?;
            let exhausted = rows.len() < 500;
            for row in rows {
                after = Some((row.name.clone(), row.version.clone()));
                let observed = row.current.as_ref().is_some_and(|c| c.source != SurfaceSource::Declared);
                let keep = match wanted {
                    "all" => true,
                    "not_observed" => !observed,
                    other => review_state(&row) == other,
                };
                if keep && out.len() < limit {
                    let mut value = row_json(&row);
                    value["member"] = json!(member.name);
                    out.push(value);
                }
            }
            if exhausted {
                break;
            }
        }
    }
    let sync = match repo.kind()? {
        RepoKind::Proxy => {
            let s = state.mcp.sync_state(repo.id).await?;
            json!({"lastRunAt": s.last_run_at.map(|t| t.to_rfc3339()), "lastError": s.last_error,
                   "skipped": s.skipped, "consecutiveFailures": s.consecutive_failures})
        }
        _ => Value::Null,
    };
    Ok(Json(json!({"repository": name, "servers": out, "sync": sync})))
}

async fn locate(state: &AppState, repo: &Repository, caller: &AuthUser, server: &str, version: &str) -> AppResult<CatalogRow> {
    let (members, _) = scope(state, repo, Some(caller)).await?;
    for member in members {
        if let Some(row) = state.mcp.version(member.id, repo.id, server, Some(version)).await? {
            return Ok(row);
        }
    }
    Err(AppError::NotFound(format!("server not found: {server}@{version}")))
}

/// GET /api/v1/mcp/{repo}/evidence?name=&version= -- every live surface
/// with its tools and header parameters, the findings with this
/// repository's suppressions marked, the decisions, the probes.
pub async fn evidence(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> AppResult<Json<Value>> {
    let caller = admin(&request)?;
    let (Some(server), Some(version)) = (params.get("name"), params.get("version")) else {
        return Err(AppError::BadRequest("name and version are required".into()));
    };
    let repo = mcp_repo(&state, &name).await?;
    let row = locate(&state, &repo, &caller, server, version).await?;
    let surfaces: Vec<Value> = state
        .mcp
        .surfaces(row.version_id)
        .await?
        .into_iter()
        .map(|s| {
            let tools: Vec<Tool> = s.tools_json.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
            let headers: Vec<Value> = tools
                .iter()
                .flat_map(|t| {
                    let hp = header_params(t);
                    let valid = hp.valid.into_iter().map({
                        let tool = t.name.clone();
                        move |p| json!({"tool": tool, "pointer": p.pointer, "header": p.header, "valid": true})
                    });
                    let invalid = hp.invalid.into_iter().map({
                        let tool = t.name.clone();
                        move |(pointer, why)| json!({"tool": tool, "pointer": pointer, "why": why, "valid": false})
                    });
                    valid.chain(invalid).collect::<Vec<_>>()
                })
                .collect();
            json!({"id": s.id, "source": s.source.as_str(), "remoteUrl": s.remote_url, "toolsSha256": s.tools_sha256,
                   "permissionsSha256": s.permissions_sha256, "capturedAt": s.captured_at.to_rfc3339(),
                   "tools": tools, "headerParams": headers})
        })
        .collect();
    let findings: Vec<Value> = state
        .mcp
        .findings_of(row.version_id, repo.id)
        .await?
        .into_iter()
        .map(|f| json!({"id": f.id, "pattern": f.pattern, "confidence": if f.high { "high" } else { "medium" },
                        "promotedBy": f.promoted_by, "field": f.field, "tool": f.tool, "span": [f.span.0, f.span.1],
                        "excerpt": f.excerpt, "suppressed": f.suppressed}))
        .collect();
    let approvals: Vec<Value> = state
        .mcp
        .approvals_of(&row.name, &row.version, false)
        .await?
        .into_iter()
        .filter(|a| a.repository == repo.id || a.repository == row.member)
        .map(|a| json!({"remoteUrl": a.remote_url, "decision": a.decision.as_str(), "decidedBy": a.decided_by,
                        "decidedAt": a.decided_at.to_rfc3339(), "note": a.note, "own": a.repository == repo.id}))
        .collect();
    let runs: Vec<Value> = state
        .mcp
        .probe_runs(row.version_id)
        .await?
        .into_iter()
        .map(|r| json!({"remoteUrl": r.remote_url, "ranAt": r.ran_at.to_rfc3339(), "ok": r.ok,
                        "protocolVersion": r.protocol_version, "error": r.error}))
        .collect();
    let record = detail_of(&row.envelope_json)?;
    Ok(Json(json!({
        "row": row_json(&row),
        "server": serde_json::to_value(&record)?,
        "permissions": crate::registry::mcp::surface::permission_surface(&record),
        "surfaces": surfaces,
        "findings": findings,
        "approvals": approvals,
        "probeRuns": runs,
    })))
}

#[derive(Deserialize)]
pub struct DecisionRequest {
    pub name: String,
    pub version: String,
    pub state: Decision,
    #[serde(default)]
    pub skill: bool,
    pub note: Option<String>,
}

/// POST /api/v1/mcp/{repo}/approvals -- approve or block a version at
/// every endpoint of its current set, or a skill's exact surface.
pub async fn decide(State(state): State<AppState>, Path(name): Path<String>, request: Request) -> AppResult<Json<Value>> {
    let caller = admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let req: DecisionRequest = body(request).await?;
    let use_case = DecideVersion::new(state.mcp.clone(), state.audit.clone(), state.events.clone());
    let now = state.clock.now();
    if req.skill {
        let (members, _) = scope(&state, &repo, Some(&caller)).await?;
        for member in members {
            if let Some(skill) = state.mcp.skill(member.id, repo.id, &req.name, &req.version).await? {
                use_case.skill(repo.id, &skill, req.state, req.note, &actor(&caller), now).await?;
                return Ok(Json(json!({"decided": 1})));
            }
        }
        return Err(AppError::NotFound(format!("skill not found: {}@{}", req.name, req.version)));
    }
    let row = locate(&state, &repo, &caller, &req.name, &req.version).await?;
    let n = use_case.server(repo.id, &row, req.state, req.note, &actor(&caller), now).await?;
    Ok(Json(json!({"decided": n})))
}
