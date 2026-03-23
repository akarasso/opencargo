use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};

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
/// Returns the first match found.
async fn find_package_by_name(
    db: &sqlx::SqlitePool,
    name: &str,
) -> Result<Option<(crate::db::Package, Vec<crate::db::Version>)>, sqlx::Error> {
    let package: Option<crate::db::Package> = sqlx::query_as(
        "SELECT * FROM packages WHERE name = ?1 LIMIT 1",
    )
    .bind(name)
    .fetch_optional(db)
    .await?;

    match package {
        Some(pkg) => {
            let versions = crate::db::get_versions(db, pkg.id).await?;
            Ok(Some((pkg, versions)))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/deps/@{scope}/{name}/dependencies
// ---------------------------------------------------------------------------

pub async fn get_dependencies(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    get_dependencies_impl(state, name).await
}

pub async fn get_dependencies_unscoped(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AppResult<impl IntoResponse> {
    get_dependencies_impl(state, name).await
}

async fn get_dependencies_impl(
    state: AppState,
    name: String,
) -> AppResult<impl IntoResponse> {
    let (pkg, versions) = find_package_by_name(&state.db, &name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {name}")))?;

    // Get the latest version (last one)
    let latest_version = versions.last().ok_or_else(|| {
        AppError::NotFound(format!("no versions found for package: {name}"))
    })?;

    let deps = crate::db::get_dependencies_for_version(&state.db, latest_version.id).await?;

    let dep_list: Vec<Value> = deps
        .iter()
        .map(|d| {
            json!({
                "name": d.dependency_name,
                "version_req": d.dependency_version_req,
                "type": d.dependency_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "package": pkg.name,
        "version": latest_version.version,
        "dependencies": dep_list,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/deps/@{scope}/{name}/dependents
// ---------------------------------------------------------------------------

pub async fn get_dependents(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    get_dependents_impl(state, name).await
}

pub async fn get_dependents_unscoped(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> AppResult<impl IntoResponse> {
    get_dependents_impl(state, name).await
}

async fn get_dependents_impl(
    state: AppState,
    name: String,
) -> AppResult<impl IntoResponse> {
    let dependents = crate::db::get_dependents(&state.db, &name).await?;

    let dep_list: Vec<Value> = dependents
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "version": d.version,
            })
        })
        .collect();

    Ok(Json(json!({
        "package": name,
        "dependents": dep_list,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/deps/@{scope}/{name}/versions/{version}/impact
// ---------------------------------------------------------------------------

pub async fn impact_analysis(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    impact_analysis_impl(state, name, version).await
}

pub async fn impact_analysis_unscoped(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    impact_analysis_impl(state, name, version).await
}

async fn impact_analysis_impl(
    state: AppState,
    name: String,
    version: String,
) -> AppResult<impl IntoResponse> {
    // Find all packages that depend on this package
    let dependents = crate::db::get_dependents(&state.db, &name).await?;

    let affected: Vec<String> = dependents
        .iter()
        .map(|d| d.name.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let safe_to_delete = affected.is_empty();

    Ok(Json(json!({
        "package": name,
        "version": version,
        "affected_packages": affected,
        "safe_to_delete": safe_to_delete,
    })))
}
