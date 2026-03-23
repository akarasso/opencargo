use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params.get("name").cloned().unwrap_or_default(),
    }
}

/// Find a package across all repositories by name.
async fn find_package_by_name(
    db: &sqlx::SqlitePool,
    name: &str,
) -> Result<Option<crate::db::Package>, sqlx::Error> {
    sqlx::query_as::<_, crate::db::Package>(
        "SELECT * FROM packages WHERE name = ?1 LIMIT 1",
    )
    .bind(name)
    .fetch_optional(db)
    .await
}

// ---------------------------------------------------------------------------
// GET /api/v1/packages/@{scope}/{name}/versions/{version}/vulns
// ---------------------------------------------------------------------------

pub async fn get_vulns(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version_str = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    get_vulns_impl(state, name, version_str).await
}

pub async fn get_vulns_unscoped(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    get_vulns_impl(state, name, version).await
}

async fn get_vulns_impl(
    state: AppState,
    name: String,
    version_str: String,
) -> AppResult<impl IntoResponse> {
    let pkg = find_package_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let version = crate::db::get_version(&state.db, pkg.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("version not found: {name}@{version_str}"))
        })?;

    let scan = crate::db::get_vulnerability_scan(&state.db, version.id).await?;

    match scan {
        Some(s) => {
            let details: serde_json::Value = s
                .scan_results_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or(json!(null));

            Ok(Json(json!({
                "package": name,
                "version": version_str,
                "scanned_at": s.scanned_at,
                "total_deps": s.total_deps,
                "vulnerable_deps": s.vulnerable_deps,
                "status": s.status,
                "details": details,
            })))
        }
        None => Ok(Json(json!({
            "package": name,
            "version": version_str,
            "scanned_at": null,
            "total_deps": 0,
            "vulnerable_deps": 0,
            "status": "not_scanned",
            "details": null,
        }))),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/packages/@{scope}/{name}/versions/{version}/rescan
// ---------------------------------------------------------------------------

pub async fn rescan(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version_str = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    rescan_impl(state, name, version_str).await
}

pub async fn rescan_unscoped(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    rescan_impl(state, name, version).await
}

async fn rescan_impl(
    state: AppState,
    name: String,
    version_str: String,
) -> AppResult<impl IntoResponse> {
    let pkg = find_package_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let version = crate::db::get_version(&state.db, pkg.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("version not found: {name}@{version_str}"))
        })?;

    // Determine ecosystem from the repository format
    let repo: crate::db::Repository = sqlx::query_as(
        "SELECT * FROM repositories WHERE id = ?1",
    )
    .bind(pkg.repository_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| AppError::Internal("failed to fetch repository".to_string()))?;

    let ecosystem = match repo.format.as_str() {
        "npm" => "npm",
        "cargo" => "crates.io",
        "go" => "Go",
        _ => "npm",
    };

    // Delete old scan results
    crate::db::delete_vulnerability_scans(&state.db, version.id).await?;

    // Run the scan
    let result = state
        .vuln_scanner
        .scan_version(&state.db, version.id, &version.metadata_json, ecosystem)
        .await
        .map_err(|e| AppError::Internal(format!("scan failed: {e}")))?;

    Ok(Json(json!({
        "package": name,
        "version": version_str,
        "total_deps": result.total_deps,
        "vulnerable_deps": result.vulnerable_deps,
        "status": result.status,
        "details": result.details,
    })))
}
