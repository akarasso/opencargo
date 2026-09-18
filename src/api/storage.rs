use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

/// GET /api/v1/system/storage — which adapter the store is, its declared
/// identity, readiness, open multipart uploads and the reclamation backlog
/// (admin only). Never an endpoint, a bucket or a path.
pub async fn storage_status(
    State(state): State<AppState>,
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
    let backlog = state.reclaim.backlog().await?;
    Ok(Json(json!({
        "backend": state.storage_backend,
        "identity": state.storage.identity().0,
        "ready": state.storage_ready.ready().await,
        "multipart_in_flight": state.multipart.in_flight().await?,
        "reclaim_candidates": backlog.candidates,
        "reclaim_prefixes": backlog.prefixes,
    })))
}
