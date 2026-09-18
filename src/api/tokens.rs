use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::api::{record_audit, require_admin_or_self, require_auth};
use crate::auth::tokens as auth_tokens;
use crate::domain::User;
use crate::error::{AppError, AppResult};
use crate::ports::tokens::NewToken;
use crate::server::AppState;
use crate::wire::wire_ts;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    pub expires_in_days: Option<i64>,
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

    let user = load_user(&state, &username).await?;
    let tokens = state.tokens.of_user(user.id).await?;

    let result: Vec<serde_json::Value> = tokens
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "prefix": t.prefix,
                "expires_at": t.expires_at.map(wire_ts),
                "last_used_at": t.last_used_at.map(wire_ts),
                "created_at": wire_ts(t.created_at),
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

    let user = load_user(&state, &username).await?;

    let token_id = uuid::Uuid::new_v4().to_string();
    let (raw_token, token_hash) = auth_tokens::generate_token("trg_");
    let prefix = &raw_token[..16];

    let now = Utc::now();
    let expires_at = body
        .expires_in_days
        .map(|days| now + chrono::Duration::days(days));

    state
        .tokens
        .create(
            &NewToken {
                id: &token_id,
                user_id: user.id,
                name: &body.name,
                prefix,
                token_hash: &token_hash,
                expires_at,
            },
            now,
        )
        .await?;

    record_audit(&state, &caller, "token.create", Some(&username)).await;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": token_id,
            "name": body.name,
            "token": raw_token,
            "prefix": prefix,
            "expires_at": expires_at.map(wire_ts),
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
    let user = load_user(&state, &username).await?;

    // Verify token belongs to this user
    let token = state
        .tokens
        .by_id(&token_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("token not found: {token_id}")))?;
    if token.user_id != user.id {
        return Err(AppError::Forbidden("token does not belong to this user".to_string()));
    }

    state.tokens.delete(&token_id).await?;

    record_audit(&state, &caller, "token.revoke", Some(&username)).await;

    Ok(Json(json!({"ok": true})))
}

/// The account the tokens belong to, or the 404 naming it.
async fn load_user(state: &AppState, username: &str) -> AppResult<User> {
    state
        .users
        .by_name(username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))
}
