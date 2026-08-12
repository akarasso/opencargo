use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::registry::extract_package_name;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Load the repository hosting a package.
async fn load_repository(
    db: &sqlx::SqlitePool,
    repository_id: i64,
) -> AppResult<crate::db::Repository> {
    sqlx::query_as("SELECT * FROM repositories WHERE id = ?1")
        .bind(repository_id)
        .fetch_one(db)
        .await
        .map_err(|_| AppError::Internal("failed to fetch repository".to_string()))
}

/// Read-access gate shared by the vulns read and rescan paths. A package whose
/// repository the caller cannot read must be indistinguishable from a package
/// that does not exist, so the denial maps to the same 404 as the name lookup.
async fn ensure_readable_or_not_found(
    db: &sqlx::SqlitePool,
    repo: &crate::db::Repository,
    auth_user: Option<&AuthUser>,
    name: &str,
) -> AppResult<()> {
    crate::registry::ensure_can_read(db, repo, auth_user)
        .await
        .map_err(|_| AppError::NotFound(format!("package not found: {name}")))
}

// ---------------------------------------------------------------------------
// GET /api/v1/packages/@{scope}/{name}/versions/{version}/vulns
// ---------------------------------------------------------------------------

pub async fn get_vulns(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version_str = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    get_vulns_impl(state, name, version_str, auth.map(|e| e.0)).await
}

pub async fn get_vulns_unscoped(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    get_vulns_impl(state, name, version, auth.map(|e| e.0)).await
}

async fn get_vulns_impl(
    state: AppState,
    name: String,
    version_str: String,
    auth_user: Option<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let pkg = find_package_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let repo = load_repository(&state.db, pkg.repository_id).await?;
    ensure_readable_or_not_found(&state.db, &repo, auth_user.as_ref(), &name).await?;

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
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version_str = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    rescan_impl(state, name, version_str, auth.map(|e| e.0)).await
}

pub async fn rescan_unscoped(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    rescan_impl(state, name, version, auth.map(|e| e.0)).await
}

async fn rescan_impl(
    state: AppState,
    name: String,
    version_str: String,
    auth_user: Option<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let pkg = find_package_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let repo = load_repository(&state.db, pkg.repository_id).await?;
    ensure_readable_or_not_found(&state.db, &repo, auth_user.as_ref(), &name).await?;

    // Rescan destroys the stored scan results and triggers outbound OSV
    // queries, so it requires write access on the repo, like publish. The
    // POST never reaches this handler anonymously (the auth middleware only
    // lets anonymous GET/HEAD through), but stay defensive.
    let caller = auth_user
        .as_ref()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    crate::registry::ensure_can_write(&state.db, &repo, caller).await?;

    let version = crate::db::get_version(&state.db, pkg.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("version not found: {name}@{version_str}"))
        })?;

    // Determine ecosystem from the repository format
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
