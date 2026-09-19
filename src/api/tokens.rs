use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::{actor, require_admin_or_self, require_auth};
use crate::app::tokens::{IssueToken, RevokeToken};
use crate::domain::{TokenScope, User};
use crate::error::{AppError, AppResult};
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

    let issued = IssueToken::new(
        state.users.clone(),
        state.tokens.clone(),
        state.ids.clone(),
        state.audit.clone(),
        state.events.clone(),
    )
    .run(
        &username,
        &body.name,
        body.expires_in_days,
        &TokenScope::Inherit,
        &actor(&caller),
        state.clock.now(),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": issued.id,
            "name": issued.name,
            "token": issued.token,
            "prefix": issued.prefix,
            "expires_at": issued.expires_at.map(wire_ts),
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

    RevokeToken::new(
        state.users.clone(),
        state.tokens.clone(),
        state.audit.clone(),
        state.events.clone(),
    )
    .run(&username, &token_id, &actor(&caller), state.clock.now())
    .await?;

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
