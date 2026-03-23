use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::auth::tokens as auth_tokens;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    pub expires_in_days: Option<i64>,
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

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/users/{username}/tokens — list tokens (admin or self)
pub async fn list_tokens(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    let tokens = crate::db::list_user_tokens(&state.db, user.id).await?;

    let result: Vec<serde_json::Value> = tokens
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "prefix": t.prefix,
                "expires_at": t.expires_at,
                "last_used_at": t.last_used_at,
                "created_at": t.created_at,
            })
        })
        .collect();

    Ok(Json(json!(result)))
}

/// POST /api/v1/users/{username}/tokens — create token (admin or self)
pub async fn create_token(
    State(state): State<AppState>,
    Path(username): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    // Rate limit: 10 token creations per minute per user
    let rate_key = format!("create_token:{}", caller.username);
    if !state.token_rate_limiter.check(&rate_key) {
        return Err(AppError::TooManyRequests(
            "too many token creation requests, try again later".to_string(),
        ));
    }

    let body: CreateTokenRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    let token_id = uuid::Uuid::new_v4().to_string();
    let (raw_token, token_hash) = auth_tokens::generate_token("trg_");
    let prefix = &raw_token[..16];

    let expires_at = body.expires_in_days.map(|days| {
        let now = chrono::Utc::now();
        let expiry = now + chrono::Duration::days(days);
        expiry.format("%Y-%m-%d %H:%M:%S").to_string()
    });

    crate::db::create_api_token(
        &state.db,
        &token_id,
        user.id,
        &body.name,
        prefix,
        &token_hash,
        expires_at.as_deref(),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": token_id,
            "name": body.name,
            "token": raw_token,
            "prefix": prefix,
            "expires_at": expires_at,
        })),
    ))
}

/// DELETE /api/v1/users/{username}/tokens/{token_id} — revoke token (admin or self)
pub async fn delete_token(
    State(state): State<AppState>,
    Path((username, token_id)): Path<(String, String)>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin_or_self(&caller, &username)?;

    // Verify the user exists
    let user = crate::db::get_user_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))?;

    // Verify token belongs to this user
    let token = crate::db::get_token_by_id(&state.db, &token_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("token not found: {token_id}")))?;
    if token.user_id != user.id {
        return Err(AppError::Forbidden("token does not belong to this user".to_string()));
    }

    crate::db::delete_token(&state.db, &token_id).await?;

    Ok(Json(json!({"ok": true})))
}
