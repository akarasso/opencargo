use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::{actor, require_admin, require_auth};
use crate::app::permissions::{RevokePermission, SetPermission};
use crate::domain::{Rights, User};
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

    let user = load_user(&state, &username).await?;
    let grants = state.permissions.of_user(user.id).await?;

    let result: Vec<serde_json::Value> = grants
        .iter()
        .map(|grant| {
            json!({
                "repository": grant.repository.clone().unwrap_or_else(|| {
                    format!("(deleted repo id={})", grant.repository_id)
                }),
                "repository_id": grant.repository_id,
                "can_read": grant.rights.read,
                "can_write": grant.rights.write,
                "can_delete": grant.rights.delete,
                "can_admin": grant.rights.admin,
            })
        })
        .collect();

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

    let rights = Rights {
        read: body.can_read.unwrap_or(true),
        write: body.can_write.unwrap_or(false),
        delete: body.can_delete.unwrap_or(false),
        admin: body.can_admin.unwrap_or(false),
    };
    SetPermission::new(
        state.users.clone(),
        state.repos.clone(),
        state.permissions.clone(),
        state.audit.clone(),
        state.events.clone(),
    )
    .run(
        &username,
        &repo_name,
        rights,
        &actor(&caller),
        chrono::Utc::now(),
    )
    .await?;

    Ok(Json(json!({
        "ok": true,
        "username": username,
        "repository": repo_name,
        "can_read": rights.read,
        "can_write": rights.write,
        "can_delete": rights.delete,
        "can_admin": rights.admin,
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

    RevokePermission::new(
        state.users.clone(),
        state.repos.clone(),
        state.permissions.clone(),
        state.audit.clone(),
        state.events.clone(),
    )
    .run(&username, &repo_name, &actor(&caller), chrono::Utc::now())
    .await?;

    Ok(Json(json!({"ok": true})))
}

async fn load_user(state: &AppState, username: &str) -> AppResult<User> {
    state
        .users
        .by_name(username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))
}
