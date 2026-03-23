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
pub struct CreateWebhookRequest {
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
    pub secret: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateWebhookRequest {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub secret: Option<String>,
    pub active: Option<bool>,
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

fn format_webhook(wh: &crate::db::Webhook) -> serde_json::Value {
    let events: Vec<String> = if wh.events == "*" {
        vec!["*".to_string()]
    } else {
        wh.events.split(',').map(|s| s.trim().to_string()).collect()
    };

    json!({
        "id": wh.id,
        "url": wh.url,
        "events": events,
        "active": wh.active != 0,
        "created_at": wh.created_at,
        "updated_at": wh.updated_at,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/webhooks -- List all webhooks (admin only)
pub async fn list_webhooks(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let webhooks = crate::db::list_webhooks(&state.db).await?;
    let result: Vec<serde_json::Value> = webhooks.iter().map(format_webhook).collect();

    Ok(Json(json!({ "webhooks": result })))
}

/// POST /api/v1/webhooks -- Create webhook (admin only)
pub async fn create_webhook(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let body: CreateWebhookRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    if body.url.is_empty() {
        return Err(AppError::BadRequest("url is required".to_string()));
    }

    let events = if body.events.is_empty() {
        "*".to_string()
    } else {
        body.events.join(",")
    };

    let id = crate::db::create_webhook(
        &state.db,
        &body.url,
        &events,
        body.secret.as_deref(),
    )
    .await?;

    let wh = crate::db::get_webhook_by_id(&state.db, id)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch created webhook".to_string()))?;

    Ok((StatusCode::CREATED, Json(format_webhook(&wh))))
}

/// PUT /api/v1/webhooks/{id} -- Update webhook (admin only)
pub async fn update_webhook(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let body: UpdateWebhookRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    if crate::db::get_webhook_by_id(&state.db, id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!("webhook not found: {id}")));
    }

    let events_str = body.events.map(|e| {
        if e.is_empty() {
            "*".to_string()
        } else {
            e.join(",")
        }
    });

    // Handle secret: if present in the request body, update it (including to null)
    let secret_update = if body.secret.is_some() {
        Some(body.secret.as_deref())
    } else {
        None
    };

    crate::db::update_webhook(
        &state.db,
        id,
        body.url.as_deref(),
        events_str.as_deref(),
        secret_update,
        body.active,
    )
    .await?;

    let updated = crate::db::get_webhook_by_id(&state.db, id)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch updated webhook".to_string()))?;

    Ok(Json(format_webhook(&updated)))
}

/// DELETE /api/v1/webhooks/{id} -- Delete webhook (admin only)
pub async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    if crate::db::get_webhook_by_id(&state.db, id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!("webhook not found: {id}")));
    }

    crate::db::delete_webhook(&state.db, id).await?;

    Ok(Json(json!({"ok": true})))
}

/// POST /api/v1/webhooks/{id}/test -- Send a test webhook (admin only)
pub async fn test_webhook(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;

    let wh = crate::db::get_webhook_by_id(&state.db, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("webhook not found: {id}")))?;

    let test_payload = json!({
        "event": "webhook.test",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "data": {
            "message": "This is a test webhook from opencargo"
        }
    });

    // Dispatch via the webhook dispatcher
    state.webhook_dispatcher.dispatch_to_url(
        &wh.url,
        wh.secret.as_deref(),
        &test_payload,
    ).await;

    Ok(Json(json!({"ok": true, "message": "test webhook sent"})))
}
