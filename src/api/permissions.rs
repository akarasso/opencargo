use axum::{
    extract::{Path, State},
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
pub struct SetPermissionRequest {
    pub can_read: Option<bool>,
    pub can_write: Option<bool>,
    pub can_delete: Option<bool>,
    pub can_admin: Option<bool>,
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

/// GET /api/v1/users/{username}/permissions -- List permissions for a user (admin only)
pub async fn list_permissions(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    let perms = crate::db::list_user_permissions(&state.db, user.id).await?;

    // Enrich with repository names
    let mut result = Vec::new();
    for perm in perms {
        // Look up repository name
        let repo_name = sqlx::query_scalar::<_, String>(
            "SELECT name FROM repositories WHERE id = ?1",
        )
        .bind(perm.repository_id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_else(|| format!("(deleted repo id={})", perm.repository_id));

        result.push(json!({
            "repository": repo_name,
            "repository_id": perm.repository_id,
            "can_read": perm.can_read != 0,
            "can_write": perm.can_write != 0,
            "can_delete": perm.can_delete != 0,
            "can_admin": perm.can_admin != 0,
        }));
    }

    Ok(Json(json!({ "permissions": result })))
}

/// PUT /api/v1/users/{username}/permissions/{repo_name} -- Set permissions (admin only)
pub async fn set_permission(
    State(state): State<AppState>,
    Path((username, repo_name)): Path<(String, String)>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let body: SetPermissionRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::db::set_user_permission(
        &state.db,
        user.id,
        repo.id,
        body.can_read.unwrap_or(true),
        body.can_write.unwrap_or(false),
        body.can_delete.unwrap_or(false),
        body.can_admin.unwrap_or(false),
    )
    .await?;

    Ok(Json(json!({
        "ok": true,
        "username": username,
        "repository": repo_name,
        "can_read": body.can_read.unwrap_or(true),
        "can_write": body.can_write.unwrap_or(false),
        "can_delete": body.can_delete.unwrap_or(false),
        "can_admin": body.can_admin.unwrap_or(false),
    })))
}

/// DELETE /api/v1/users/{username}/permissions/{repo_name} -- Remove permissions (admin only)
pub async fn delete_permission(
    State(state): State<AppState>,
    Path((username, repo_name)): Path<(String, String)>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::db::delete_user_permission(&state.db, user.id, repo.id).await?;

    Ok(Json(json!({"ok": true})))
}
