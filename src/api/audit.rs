use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AuditQuery {
    pub page: Option<i64>,
    pub size: Option<i64>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// GET /api/v1/system/audit — list audit entries (admin only)
pub async fn list_audit(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    if caller.role != "admin" {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }

    let page = query.page.unwrap_or(1).max(1);
    let size = query.size.unwrap_or(50).min(200).max(1);

    let entries = crate::db::list_audit_entries(&state.db, page, size).await?;

    let result: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "user_id": e.user_id,
                "username": e.username,
                "action": e.action,
                "target": e.target,
                "repository": e.repository,
                "ip": e.ip,
                "user_agent": e.user_agent,
                "details_json": e.details_json,
                "created_at": e.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "entries": result,
        "page": page,
        "size": size,
    })))
}
