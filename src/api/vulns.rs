use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde_json::json;

use crate::app::scan::ScanVersion;
use crate::auth::middleware::AuthUser;
use crate::domain::Repository;
use crate::error::{AppError, AppResult};
use crate::ports::vulns::ScanError;
use crate::registry::extract_package_name;
use crate::server::AppState;
use crate::wire::wire_ts;

/// The repository hosting a package. A package always has one, so its
/// absence is this server's inconsistency, not the caller's mistake.
async fn load_repository(state: &AppState, repository_id: i64) -> AppResult<Repository> {
    state
        .repos
        .by_id(repository_id)
        .await?
        .ok_or_else(|| AppError::Internal("failed to fetch repository".to_string()))
}

/// The version, or the same 404 a missing package answers with.
async fn version_of(
    state: &AppState,
    package: i64,
    name: &str,
    version: &str,
) -> AppResult<crate::domain::Version> {
    state
        .packages
        .version(package, version)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("version not found: {name}@{version}")))
}

/// Read-access gate shared by the vulns read and rescan paths. A package whose
/// repository the caller cannot read must be indistinguishable from a package
/// that does not exist, so the denial maps to the same 404 as the name lookup.
async fn ensure_readable_or_not_found(
    authz: &crate::app::authorize::Authorize<'_>,
    repo: &crate::domain::Repository,
    auth_user: Option<&AuthUser>,
    name: &str,
) -> AppResult<()> {
    crate::registry::ensure_can_read(authz, repo, auth_user)
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
    let pkg = state
        .packages
        .anywhere(&name, false)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let repo = load_repository(&state, pkg.repository_id).await?;
    ensure_readable_or_not_found(&state.authorize(), &repo, auth_user.as_ref(), &name).await?;

    let version = version_of(&state, pkg.id, &name, &version_str).await?;
    let scan = state.vulns.latest(version.id).await?;

    match scan {
        Some(s) => {
            Ok(Json(json!({
                "package": name,
                "version": version_str,
                "scanned_at": wire_ts(s.scanned_at),
                "total_deps": s.total_deps,
                "vulnerable_deps": s.vulnerable_deps,
                "status": s.status,
                "details": s.details.unwrap_or(json!(null)),
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
    let pkg = state
        .packages
        .anywhere(&name, false)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    let repo = load_repository(&state, pkg.repository_id).await?;
    ensure_readable_or_not_found(&state.authorize(), &repo, auth_user.as_ref(), &name).await?;

    // Rescan destroys the stored scan results and triggers outbound OSV
    // queries, so it requires write access on the repo, like publish. The
    // POST never reaches this handler anonymously (the auth middleware only
    // lets anonymous GET/HEAD through), but stay defensive.
    let caller = auth_user
        .as_ref()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, caller).await?;

    let version = version_of(&state, pkg.id, &name, &version_str).await?;

    let format = repo.fmt()?;
    let ecosystem = format.osv_ecosystem().ok_or_else(|| {
        AppError::BadRequest(format!(
            "vulnerability scanning is not available for {} repositories",
            format.as_str()
        ))
    })?;

    let scan = ScanVersion::new(state.vuln_scanner.clone(), state.vulns.clone());
    scan.forget(version.id).await?;
    let result = scan
        .run(version.id, &version.metadata_json, ecosystem, Utc::now())
        .await
        .map_err(|e| match e {
            ScanError::Unscannable(why) => AppError::BadRequest(format!("cannot scan: {why}")),
            other => AppError::ServiceUnavailable(format!("scan failed: {other}")),
        })?;

    Ok(Json(json!({
        "package": name,
        "version": version_str,
        "total_deps": result.total_deps,
        "vulnerable_deps": result.vulnerable_deps,
        "status": result.status,
        "details": result.details,
    })))
}
