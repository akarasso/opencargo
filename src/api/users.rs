use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::api::{actor, require_admin, require_admin_or_self, require_auth};
use crate::app::users::{
    AccountPatch, ChangePassword, CreateUser, DeleteUser, NewAccount, UpdateUser,
};
use crate::domain::User;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::wire::wire_ts;

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
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/users — list all users (admin only)
pub async fn list_users(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let users = state.users.all().await?;
    let result: Vec<serde_json::Value> = users.iter().map(described).collect();

    Ok(Json(json!(result)))
}

/// POST /api/v1/users — create a user (admin only). The password is generated
/// server-side and returned once; any password in the request is ignored.
pub async fn create_user(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Extract auth first, then consume body
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: CreateUserRequest = read_json(request).await?;

    let created = CreateUser::new(state.users.clone(), state.audit.clone(), state.events.clone())
        .run(
            &NewAccount {
                username: &body.username,
                email: body.email.as_deref(),
                role: body.role.as_deref(),
            },
            &actor(&caller),
            Utc::now(),
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "username": created.user.username,
            "email": created.user.email,
            "role": created.user.role,
            "password": created.password,
            "created_at": wire_ts(created.user.created_at),
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

    let user = load_user(&state, &username).await?;

    Ok(Json(described(&user)))
}

/// PUT /api/v1/users/{username} — update user (admin or self)
pub async fn update_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;
    let body: UpdateUserRequest = read_json(request).await?;

    let user = UpdateUser::new(state.users.clone(), state.audit.clone(), state.events.clone())
        .run(
            &username,
            &AccountPatch {
                email: body.email.as_deref(),
                password: body.password.as_deref(),
                role: body.role.as_deref(),
            },
            &actor(&caller),
            Utc::now(),
        )
        .await?;

    Ok(Json(described(&user)))
}

/// DELETE /api/v1/users/{username} — delete user (admin only)
pub async fn delete_user(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    DeleteUser::new(state.users.clone(), state.audit.clone(), state.events.clone())
        .run(&username, &actor(&caller), Utc::now())
        .await?;

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
    let body: ChangePasswordRequest = read_json(request).await?;

    ChangePassword::new(state.users.clone())
        .run(
            &username,
            body.current_password.as_deref(),
            &body.new_password,
            &actor(&caller),
            Utc::now(),
        )
        .await?;

    Ok(Json(json!({"ok": true})))
}

async fn read_json<T: serde::de::DeserializeOwned>(
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The account, or the 404 naming it.
async fn load_user(state: &AppState, username: &str) -> AppResult<User> {
    state
        .users
        .by_name(username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))
}

/// How an account reaches a client: never its hash, and its timestamps as
/// RFC 3339 rather than in the store's spelling.
fn described(user: &User) -> serde_json::Value {
    json!({
        "username": user.username,
        "email": user.email,
        "role": user.role,
        "created_at": wire_ts(user.created_at),
        "updated_at": wire_ts(user.updated_at),
    })
}
