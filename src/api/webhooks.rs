use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::{require_admin, require_auth};
use crate::app::webhooks::{CreateWebhook, NewHook};
use crate::domain::{Subscription, Webhook};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::webhooks::WebhookPatch;
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

fn format_webhook(wh: &Webhook) -> serde_json::Value {
    json!({
        "id": wh.id,
        "url": wh.url,
        "events": wh.events.names(),
        "active": wh.active,
        "created_at": wh.created_at,
        "updated_at": wh.updated_at,
    })
}

/// The store's "no such row" in this endpoint's words.
fn missing(err: StoreError, id: i64) -> AppError {
    match err {
        StoreError::NotFound => AppError::NotFound(format!("webhook not found: {id}")),
        other => other.into(),
    }
}

async fn read_body<T: serde::de::DeserializeOwned>(
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    Ok(serde_json::from_slice(&bytes)?)
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

    let webhooks = state.webhooks.all().await?;
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

    let body: CreateWebhookRequest = read_body(request).await?;
    let wh = CreateWebhook::new(state.webhooks.clone())
        .run(
            &NewHook {
                url: body.url,
                events: body.events,
                secret: body.secret,
            },
            chrono::Utc::now(),
        )
        .await?;

    crate::api::record_audit(&state, &caller, "webhook.create", Some(&wh.url)).await;

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

    let body: UpdateWebhookRequest = read_body(request).await?;
    let events = body.events.map(|names| Subscription::of_names(&names));
    let updated = state
        .webhooks
        .update(
            id,
            &WebhookPatch {
                url: body.url.as_deref(),
                events: events.as_ref(),
                // A secret absent from the body is left alone; one present is
                // written.
                secret: body.secret.as_deref().map(Some),
                active: body.active,
            },
            chrono::Utc::now(),
        )
        .await
        .map_err(|err| missing(err, id))?;

    crate::api::record_audit(&state, &caller, "webhook.update", Some(&updated.url)).await;

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

    state
        .webhooks
        .delete(id)
        .await
        .map_err(|err| missing(err, id))?;

    crate::api::record_audit(&state, &caller, "webhook.delete", Some(&id.to_string())).await;

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

    let wh = state
        .webhooks
        .by_id(id)
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
    state
        .webhook_dispatcher
        .dispatch_to_url(&wh.url, wh.secret.as_deref(), &test_payload)
        .await;

    Ok(Json(json!({"ok": true, "message": "test webhook sent"})))
}
