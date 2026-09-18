use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{require_admin, require_auth};
use crate::db::kinds::validate_spec;
use crate::domain::{Format, RepoConfig, RepoKind, RepoSpec, Repository, Visibility};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::repositories::RepoPatch;
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
    // The API never exposes pypi: there is no registry module behind it.
    if format == Format::Pypi {
        return Err(AppError::BadRequest(format!(
            "invalid repository format: {}",
            body.format
        )));
    }
    let visibility: Visibility = body.visibility.parse()?;

    if state.repos.by_name(&body.name).await?.is_some() {
        return Err(AppError::Conflict(format!(
            "repository already exists: {}",
            body.name
        )));
    }

    let members = body.members.unwrap_or_default();
    let spec = RepoSpec {
        name: &body.name,
        kind,
        format,
        visibility,
        upstream: non_empty(body.upstream.as_deref()),
        members: &members,
    };
    validate_spec(state.repos.as_ref(), &spec, &[]).await?;

    let repo = state.repos.create(&spec, chrono::Utc::now()).await?;

    crate::api::record_audit(&state, &caller, "repo.create", Some(&repo.name)).await;
    emit_repositories_changed(&state);

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

/// PUT /api/v1/repositories/{name} -- Update repository (admin only). The
/// patch is merged onto the stored row and validated like a create; a new
/// upstream purges the cache first, since rows are not keyed by upstream.
pub async fn update_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: UpdateRepositoryRequest = read_json(request).await?;

    let repo = load_repo(&state, &name).await?;
    let visibility = body
        .visibility
        .as_deref()
        .map(str::parse::<Visibility>)
        .transpose()?;

    let upstream_patch = non_empty(body.upstream.as_deref());
    let members = body.members.clone().unwrap_or_else(|| repo.members());
    let spec = RepoSpec {
        name: &repo.name,
        kind: repo.kind()?,
        format: repo.fmt()?,
        visibility: visibility.unwrap_or(repo.visibility),
        upstream: upstream_patch.or(repo.upstream_url.as_deref()),
        members: &members,
    };
    validate_spec(state.repos.as_ref(), &spec, &[]).await?;
    if upstream_patch.is_some_and(|u| Some(u) != repo.upstream_url.as_deref()) {
        purge_repository(&state, &repo).await?;
    }

    let config = body
        .members
        .is_some()
        .then(|| RepoConfig::of_members(&members));
    let updated = state
        .repos
        .update(
            &name,
            &RepoPatch {
                visibility,
                upstream: upstream_patch,
                config: config.as_ref(),
            },
            chrono::Utc::now(),
        )
        .await?;

    crate::api::record_audit(&state, &caller, "repo.update", Some(&name)).await;
    emit_repositories_changed(&state);

    Ok(Json(repo_json(&updated)))
}

/// DELETE /api/v1/repositories/{name} -- Delete repository (admin only):
/// refused while packages or a group membership remain, a proxy's cache
/// purged first so the row can go. A group owns no cache: its members keep
/// theirs.
pub async fn delete_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let repo = load_repo(&state, &name).await?;

    let holders = groups_containing(&state, &name).await?;
    if !holders.is_empty() {
        return Err(AppError::Conflict(format!(
            "repository '{name}' is a member of group(s) {}; remove it from them first",
            holders.join(", ")
        )));
    }

    let kind = repo.kind()?;
    state
        .repos
        .delete_empty(&name)
        .await
        .map_err(|err| match err {
            StoreError::Conflict => AppError::Conflict(format!(
                "repository '{name}' is not empty; delete its packages first"
            )),
            other => other.into(),
        })?;

    // The cached files go after the commit and outside it: a store method
    // touches nothing but the database, and a refused delete purges nothing.
    if kind == RepoKind::Proxy {
        purge_repository(&state, &repo).await?;
    }

    crate::api::record_audit(&state, &caller, "repo.delete", Some(&name)).await;
    emit_repositories_changed(&state);

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
    purge_repository(&state, &repo).await?;

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

/// Names of the groups listing `name` as a member.
async fn groups_containing(state: &AppState, name: &str) -> AppResult<Vec<String>> {
    let repos = state.repos.all().await?;
    let mut holders = Vec::new();
    for repo in repos {
        if repo.kind()? == RepoKind::Group && repo.members().iter().any(|m| m == name) {
            holders.push(repo.name);
        }
    }
    Ok(holders)
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

/// Public hint that the repository list changed. Payload-free on purpose: the
/// anonymous-visible repo list is already public, and every client refetches
/// through the REST API which enforces per-caller filtering.
fn emit_repositories_changed(state: &AppState) {
    state.events.emit(
        "repositories.changed",
        crate::events::Visibility::Public,
        serde_json::json!({}),
    );
}
