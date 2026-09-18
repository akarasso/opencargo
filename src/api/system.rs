use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::backup::{incomplete_snapshots, LAST_BACKUP_AT, LAST_BACKUP_TO, LAST_BACKUP_WAL};
use crate::error::{AppError, AppResult};
use crate::ports::leases::WRITER;
use crate::server::AppState;
use crate::telemetry::cleanup::LAST_SWEEP_AT;

/// GET /api/v1/system/instance — the one instance: its lease, its last
/// backup and sweep, its shutdown bounds (admin only). No host, no path, no
/// bucket. `open_http_connections` excludes WebSocket clients, which leave
/// the count at their upgrade.
pub async fn instance_status(
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
    let row = state.leases.current(WRITER).await?;
    let get = |name: &'static str| {
        let store = state.server_state.clone();
        async move { store.get(name).await }
    };
    let backup_dir = match &state.instance.backup_to {
        Some(to) => Some(to.clone()),
        None => get(LAST_BACKUP_TO).await?.map(std::path::PathBuf::from),
    };
    let incomplete = backup_dir
        .map(|dir| incomplete_snapshots(&dir).map(|(n, _)| n).unwrap_or(0))
        .unwrap_or(0);
    let owner: String = state.lease.owner().chars().take(8).collect();
    Ok(Json(json!({
        "owner": owner,
        "version": env!("CARGO_PKG_VERSION"),
        "acquired_at": row.as_ref().map(|r| r.acquired_at.to_rfc3339()),
        "renewed_at": row.as_ref().map(|r| r.renewed_at.to_rfc3339()),
        "lease": state.lease.status().as_str(),
        "last_backup_at": get(LAST_BACKUP_AT).await?,
        "last_sweep_at": get(LAST_SWEEP_AT).await?,
        "last_backup_wal": get(LAST_BACKUP_WAL).await?,
        "incomplete_snapshots": incomplete,
        "shutdown_grace_secs": state.instance.shutdown_grace_secs,
        "endpoint_drain_secs": state.instance.endpoint_drain_secs,
        "open_http_connections": state.srv.connection_count(),
    })))
}
