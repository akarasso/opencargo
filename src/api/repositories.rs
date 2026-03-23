use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

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
// Helpers
// ---------------------------------------------------------------------------

fn require_auth(request: &axum::http::Request<axum::body::Body>) -> AppResult<AuthUser> {
    request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

fn require_admin(caller: &AuthUser) -> AppResult<()> {
    if caller.role != "admin" {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }
    Ok(())
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

    let body: CreateRepositoryRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    // Validate repo_type
    if !matches!(body.repo_type.as_str(), "hosted" | "proxy" | "group") {
        return Err(AppError::BadRequest(format!(
            "invalid repository type: {}",
            body.repo_type
        )));
    }

    // Validate format
    if !matches!(
        body.format.as_str(),
        "npm" | "cargo" | "oci" | "go" | "pypi"
    ) {
        return Err(AppError::BadRequest(format!(
            "invalid repository format: {}",
            body.format
        )));
    }

    // Validate visibility
    if !matches!(body.visibility.as_str(), "public" | "private") {
        return Err(AppError::BadRequest(format!(
            "invalid visibility: {}",
            body.visibility
        )));
    }

    // Check that repo does not already exist
    if crate::db::get_repository_by_name(&state.db, &body.name)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "repository already exists: {}",
            body.name
        )));
    }

    // Build config_json for group repos
    let config_json = if body.repo_type == "group" {
        let members = body.members.unwrap_or_default();
        Some(serde_json::json!({ "members": members }).to_string())
    } else {
        None
    };

    let _id = crate::db::create_repository(
        &state.db,
        &body.name,
        &body.repo_type,
        &body.format,
        &body.visibility,
        body.upstream.as_deref(),
        config_json.as_deref(),
    )
    .await?;

    let repo = crate::db::get_repository_by_name(&state.db, &body.name)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch created repository".to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": repo.id,
            "name": repo.name,
            "type": repo.repo_type,
            "format": repo.format,
            "visibility": repo.visibility,
            "upstream": repo.upstream_url,
            "config": repo.config_json,
            "created_at": repo.created_at,
            "updated_at": repo.updated_at,
        })),
    ))
}

/// GET /api/v1/repositories/{name} -- Get repository details (any authenticated user)
pub async fn get_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let _caller = require_auth(&request)?;

    let repo = crate::db::get_repository_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))?;

    Ok(Json(json!({
        "id": repo.id,
        "name": repo.name,
        "type": repo.repo_type,
        "format": repo.format,
        "visibility": repo.visibility,
        "upstream": repo.upstream_url,
        "config": repo.config_json,
        "created_at": repo.created_at,
        "updated_at": repo.updated_at,
    })))
}

/// PUT /api/v1/repositories/{name} -- Update repository (admin only)
pub async fn update_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let body: UpdateRepositoryRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    let _repo = crate::db::get_repository_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))?;

    if let Some(ref vis) = body.visibility {
        if !matches!(vis.as_str(), "public" | "private") {
            return Err(AppError::BadRequest(format!("invalid visibility: {vis}")));
        }
    }

    let config_json = body.members.map(|members| {
        serde_json::json!({ "members": members }).to_string()
    });

    crate::db::update_repository(
        &state.db,
        &name,
        body.visibility.as_deref(),
        body.upstream.as_deref(),
        config_json.as_deref(),
    )
    .await?;

    let updated = crate::db::get_repository_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch updated repository".to_string()))?;

    Ok(Json(json!({
        "id": updated.id,
        "name": updated.name,
        "type": updated.repo_type,
        "format": updated.format,
        "visibility": updated.visibility,
        "upstream": updated.upstream_url,
        "config": updated.config_json,
        "created_at": updated.created_at,
        "updated_at": updated.updated_at,
    })))
}

/// DELETE /api/v1/repositories/{name} -- Delete repository (admin only)
pub async fn delete_repository(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    if crate::db::get_repository_by_name(&state.db, &name)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!("repository not found: {name}")));
    }

    crate::db::delete_repository(&state.db, &name).await?;

    Ok(Json(json!({"ok": true})))
}

/// POST /api/v1/repositories/{name}/purge-cache -- Purge proxy cache (admin only)
pub async fn purge_cache(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let repo = crate::db::get_repository_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))?;

    if repo.repo_type != "proxy" {
        return Err(AppError::BadRequest(
            "can only purge cache on proxy repositories".to_string(),
        ));
    }

    // Delete all proxy cache metadata for this repository
    sqlx::query("DELETE FROM proxy_cache_meta WHERE repository_id = ?1")
        .bind(repo.id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({"ok": true, "message": format!("cache purged for repository: {name}")})))
}
