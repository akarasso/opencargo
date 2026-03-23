use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::auth::users as auth_users;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub email: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub email: Option<String>,
    pub password: Option<String>,
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: Option<String>,
    pub new_password: String,
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

fn require_admin_or_self(caller: &AuthUser, target_username: &str) -> AppResult<()> {
    if caller.role != "admin" && caller.username != target_username {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }
    Ok(())
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

/// GET /api/v1/users — list all users (admin only)
pub async fn list_users(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let users = crate::db::list_users(&state.db).await?;
    let result: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            json!({
                "username": u.username,
                "email": u.email,
                "role": u.role,
                "created_at": u.created_at,
                "updated_at": u.updated_at,
            })
        })
        .collect();

    Ok(Json(json!(result)))
}

/// POST /api/v1/users — create a user (admin only)
pub async fn create_user(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Extract auth first, then consume body
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let body: CreateUserRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    // Check that user does not already exist
    if crate::db::get_user_by_username(&state.db, &body.username)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "user already exists: {}",
            body.username
        )));
    }

    let role = body.role.as_deref().unwrap_or("reader");
    if !matches!(role, "admin" | "publisher" | "reader") {
        return Err(AppError::BadRequest(format!("invalid role: {role}")));
    }

    // Always generate a random password (ignore any password in the request)
    let raw_password = auth_users::generate_random_password();
    let password_hash = auth_users::hash_password(&raw_password)
        .map_err(|e| AppError::Internal(format!("failed to hash password: {e}")))?;

    crate::db::create_user(
        &state.db,
        &body.username,
        body.email.as_deref(),
        &password_hash,
        role,
    )
    .await?;

    let user = crate::db::get_user_by_username(&state.db, &body.username)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch created user".to_string()))?;

    // No forced password change — the admin receives the generated password
    // and transmits it securely to the user. Only the initial admin account
    // (created at first startup) requires a password change.

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "username": user.username,
            "email": user.email,
            "role": user.role,
            "password": raw_password,
            "created_at": user.created_at,
        })),
    ))
}

/// GET /api/v1/users/{username} — get user (admin or self)
pub async fn get_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    Ok(Json(json!({
        "username": user.username,
        "email": user.email,
        "role": user.role,
        "created_at": user.created_at,
        "updated_at": user.updated_at,
    })))
}

/// PUT /api/v1/users/{username} — update user (admin or self)
pub async fn update_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    let body: UpdateUserRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    // Check user exists
    if crate::db::get_user_by_username(&state.db, &username)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!("user not found: {username}")));
    }

    // Only admins can change roles
    if body.role.is_some() && caller.role != "admin" {
        return Err(AppError::Forbidden(
            "only admins can change roles".to_string(),
        ));
    }

    if let Some(ref role) = body.role {
        if !matches!(role.as_str(), "admin" | "publisher" | "reader") {
            return Err(AppError::BadRequest(format!("invalid role: {role}")));
        }
    }

    let password_hash = match &body.password {
        Some(pw) => Some(
            auth_users::hash_password(pw)
                .map_err(|e| AppError::Internal(format!("failed to hash password: {e}")))?,
        ),
        None => None,
    };

    crate::db::update_user(
        &state.db,
        &username,
        body.email.as_deref(),
        password_hash.as_deref(),
        body.role.as_deref(),
    )
    .await?;

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch updated user".to_string()))?;

    Ok(Json(json!({
        "username": user.username,
        "email": user.email,
        "role": user.role,
        "created_at": user.created_at,
        "updated_at": user.updated_at,
    })))
}

/// DELETE /api/v1/users/{username} — delete user (admin only)
pub async fn delete_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    if crate::db::get_user_by_username(&state.db, &username)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!("user not found: {username}")));
    }

    crate::db::delete_user(&state.db, &username).await?;

    Ok(Json(json!({"ok": true})))
}

/// PUT /api/v1/users/{username}/password — change password (admin or self)
pub async fn change_password(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    let body: ChangePasswordRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    // Non-admin callers must provide their current password
    if caller.role != "admin" {
        let current = body.current_password.as_deref().ok_or_else(|| {
            AppError::BadRequest("current_password is required".to_string())
        })?;
        let ok = auth_users::verify_password(current, &user.password_hash)
            .map_err(|e| AppError::Internal(format!("failed to verify password: {e}")))?;
        if !ok {
            return Err(AppError::Unauthorized("invalid current password".to_string()));
        }
    }

    let new_hash = auth_users::hash_password(&body.new_password)
        .map_err(|e| AppError::Internal(format!("failed to hash password: {e}")))?;

    crate::db::update_user(&state.db, &username, None, Some(&new_hash), None).await?;
    crate::db::set_must_change_password(&state.db, user.id, false).await?;

    Ok(Json(json!({"ok": true})))
}
