use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{actor, require_admin, require_auth};
use crate::app::repositories::{
    CreateRepository, DeleteRepository, RepoUpdate, UpdateRepository,
};
use crate::domain::{Format, RepoKind, RepoSpec, Repository, Visibility};
use crate::error::{AppError, AppResult};
use crate::proxy::purge::purge_repository;
use crate::server::AppState;
use crate::wire::{wire_config, wire_ts};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateRepositoryRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub repo_type: String,
    pub format: String,
    #[serde(default = "default_visibility")]
    pub visibility: String,
    pub upstream: Option<String>,
    pub members: Option<Vec<String>>,
}

fn default_visibility() -> String {
    "private".to_string()
}

#[derive(Deserialize)]
pub struct UpdateRepositoryRequest {
    pub visibility: Option<String>,
    pub upstream: Option<String>,
    pub members: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/repositories -- Create a new repository (admin only)
pub async fn create_repository(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: CreateRepositoryRequest = read_json(request).await?;
    let kind: RepoKind = body.repo_type.parse()?;
    let format: Format = body.format.parse()?;
    let visibility: Visibility = body.visibility.parse()?;
    let members = body.members.unwrap_or_default();

    let repo = CreateRepository::new(state.repos.clone(), state.audit.clone(), state.events.clone())
        .guarding(state.storage.clone())
        .run(
            &RepoSpec {
                name: &body.name,
                kind,
                format,
                visibility,
                upstream: non_empty(body.upstream.as_deref()),
                members: &members,
            },
            &actor(&caller),
            chrono::Utc::now(),
        )
        .await?;

    Ok((StatusCode::CREATED, Json(repo_json(&repo))))
}

/// GET /api/v1/repositories/{name} -- Get repository details (authenticated
/// caller with read access; the response includes `upstream_url` and
/// `config_json`, which must not leak to callers lacking read on the repo)
pub async fn get_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;

    let repo = load_repo(&state, &name).await?;

    // A private repo the caller cannot read must be indistinguishable from a
    // missing one, so the permission denial maps to the same 404.
    crate::registry::ensure_can_read(&*state.permissions, &repo, Some(&caller))
        .await
        .map_err(|_| AppError::NotFound(format!("repository not found: {name}")))?;

    Ok(Json(repo_json(&repo)))
}

/// PUT /api/v1/repositories/{name} -- Update repository (admin only)
pub async fn update_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: UpdateRepositoryRequest = read_json(request).await?;
    let visibility = body
        .visibility
        .as_deref()
        .map(str::parse::<Visibility>)
        .transpose()?;

    let updated = UpdateRepository::new(
        state.repos.clone(),
        state.audit.clone(),
        state.events.clone(),
        state.proxy.clone(),
    )
    .run(
        &name,
        &RepoUpdate {
            visibility,
            upstream: non_empty(body.upstream.as_deref()),
            members: body.members.as_deref(),
        },
        &actor(&caller),
        chrono::Utc::now(),
    )
    .await?;

    Ok(Json(repo_json(&updated)))
}

/// DELETE /api/v1/repositories/{name} -- Delete repository (admin only)
pub async fn delete_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    DeleteRepository::new(
        state.repos.clone(),
        state.audit.clone(),
        state.events.clone(),
        Arc::new(state.reclaim_orphans()),
    )
    .run(&name, &actor(&caller), chrono::Utc::now())
    .await?;

    Ok(Json(json!({"ok": true})))
}

/// POST /api/v1/repositories/{name}/purge-cache -- Purge the proxy cache of a
/// proxy repository or of every proxy member of a group (admin only)
pub async fn purge_cache(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let repo = load_repo(&state, &name).await?;
    purge_repository(&state.proxy, state.repos.as_ref(), &repo).await?;

    crate::api::record_audit(&state, &caller, "repo.purge_cache", Some(&name)).await;

    Ok(Json(json!({"ok": true, "message": format!("cache purged for repository: {name}")})))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn read_json<T: serde::de::DeserializeOwned>(
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn load_repo(state: &AppState, name: &str) -> AppResult<Repository> {
    state
        .repos
        .by_name(name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))
}

/// An empty upstream string is treated as absent, as the UI sends it.
fn non_empty(upstream: Option<&str>) -> Option<&str> {
    upstream.filter(|u| !u.is_empty())
}

fn repo_json(repo: &Repository) -> Value {
    json!({
        "id": repo.id,
        "name": repo.name,
        "type": repo.repo_type,
        "format": repo.format,
        "visibility": repo.visibility,
        "upstream": repo.upstream_url,
        "config": wire_config(repo.config.as_ref()),
        "created_at": wire_ts(repo.created_at),
        "updated_at": wire_ts(repo.updated_at),
    })
}
