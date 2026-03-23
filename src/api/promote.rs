use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::can_admin;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PromoteRequest {
    pub from: String,
    pub to: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the full package name from path parameters.
/// For scoped packages: scope="trace", name="httpclient" -> "@trace/httpclient"
/// For unscoped packages: name="react" -> "react"
fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params.get("name").cloned().unwrap_or_default(),
    }
}

/// Rewrite the dist.tarball URL in version metadata to point to the target repo.
fn rewrite_tarball_url(
    metadata_json: &str,
    base_url: &str,
    from_repo: &str,
    to_repo: &str,
) -> String {
    let mut meta: Value = serde_json::from_str(metadata_json).unwrap_or(json!({}));

    if let Some(dist) = meta.get_mut("dist") {
        if let Some(obj) = dist.as_object_mut() {
            if let Some(tarball_val) = obj.get("tarball").cloned() {
                if let Some(tarball_url) = tarball_val.as_str() {
                    // Replace the repo name prefix in the tarball URL
                    let from_prefix = format!("{}/{}/", base_url, from_repo);
                    let to_prefix = format!("{}/{}/", base_url, to_repo);
                    let new_url = tarball_url.replacen(&from_prefix, &to_prefix, 1);
                    obj.insert("tarball".to_string(), Value::String(new_url));
                }
            }
        }
    }

    serde_json::to_string(&meta).unwrap_or_else(|_| metadata_json.to_string())
}

// ---------------------------------------------------------------------------
// POST /api/v1/packages/@{scope}/{name}/versions/{version}/promote
// ---------------------------------------------------------------------------

pub async fn promote_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    promote_impl(state, name, version, request).await
}

/// POST /api/v1/packages/{name}/versions/{version}/promote (unscoped)
pub async fn promote_package_unscoped(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    promote_impl(state, name, version, request).await
}

async fn promote_impl(
    state: AppState,
    name: String,
    version: String,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // 1. Require authentication + admin role
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    if !can_admin(&auth_user.role) {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }

    // Parse the request body
    let body: PromoteRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    // 2. Validate both repos exist and are hosted with the same format
    let from_repo = crate::db::get_repository_by_name(&state.db, &body.from)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("source repository not found: {}", body.from))
        })?;

    let to_repo = crate::db::get_repository_by_name(&state.db, &body.to)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("target repository not found: {}", body.to))
        })?;

    if from_repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(format!(
            "source repository '{}' is not a hosted repository",
            body.from
        )));
    }

    if to_repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(format!(
            "target repository '{}' is not a hosted repository",
            body.to
        )));
    }

    if from_repo.format != to_repo.format {
        return Err(AppError::BadRequest(format!(
            "cannot promote across formats: source is '{}', target is '{}'",
            from_repo.format, to_repo.format
        )));
    }

    // 3. Look up the package+version in the source repo
    let from_package = crate::db::get_package(&state.db, from_repo.id, &name)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "package '{}' not found in repository '{}'",
                name, body.from
            ))
        })?;

    let from_version = crate::db::get_version(&state.db, from_package.id, &version)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version '{}' of package '{}' not found in repository '{}'",
                version, name, body.from
            ))
        })?;

    // 4. Create the package in the target repo if it doesn't exist
    let to_package = match crate::db::get_package(&state.db, to_repo.id, &name).await? {
        Some(p) => p,
        None => {
            let _id = crate::db::create_package(
                &state.db,
                to_repo.id,
                &name,
                from_package.description.as_deref(),
            )
            .await?;
            crate::db::get_package(&state.db, to_repo.id, &name)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!(
                        "failed to create package '{}' in target repo",
                        name
                    ))
                })?
        }
    };

    // 5. Check the version doesn't already exist in target (409 Conflict)
    if crate::db::get_version(&state.db, to_package.id, &version)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "version '{}' already exists in repository '{}'",
            version, body.to
        )));
    }

    // 6. Create a new version record pointing to the same tarball_path (no file copy)
    // 7. Rewrite the dist.tarball URL to point to the target repo
    let metadata_json =
        rewrite_tarball_url(&from_version.metadata_json, &state.base_url, &body.from, &body.to);

    let new_version_id = crate::db::create_version(
        &state.db,
        to_package.id,
        &version,
        &metadata_json,
        from_version.checksum_sha1.as_deref(),
        from_version.checksum_sha256.as_deref(),
        from_version.integrity.as_deref(),
        from_version.size,
        &from_version.tarball_path, // same tarball path - no copy
    )
    .await?;

    // 8. Copy dist-tags from the source version
    let from_dist_tags = crate::db::get_dist_tags(&state.db, from_package.id).await?;
    for dt in &from_dist_tags {
        if dt.version_id == from_version.id {
            crate::db::set_dist_tag(&state.db, to_package.id, &dt.tag, new_version_id).await?;
        }
    }

    // 9. Create an audit log entry
    let details = json!({
        "from": body.from,
        "to": body.to,
    });
    let target_str = format!("{}@{}", name, version);

    crate::db::create_audit_entry(
        &state.db,
        auth_user.user_id,
        Some(&auth_user.username),
        "package.promote",
        Some(&target_str),
        Some(&body.to),
        None,
        None,
        Some(&details.to_string()),
    )
    .await?;

    // Dispatch webhook for package.promoted
    state.webhook_dispatcher.dispatch("package.promoted", &json!({
        "package": name,
        "version": version,
        "from": body.from,
        "to": body.to,
        "promoted_by": auth_user.username,
    })).await;

    info!(
        package = %name,
        version = %version,
        from = %body.from,
        to = %body.to,
        "Package version promoted"
    );

    // 10. Return success
    Ok(Json(json!({
        "ok": true,
        "package": name,
        "version": version,
        "from": body.from,
        "to": body.to,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/packages/@{scope}/{name}/versions/{version}/promotions
// ---------------------------------------------------------------------------

pub async fn list_promotions(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    list_promotions_impl(state, name, version, request).await
}

/// GET /api/v1/packages/{name}/versions/{version}/promotions (unscoped)
pub async fn list_promotions_unscoped(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    list_promotions_impl(state, name, version, request).await
}

async fn list_promotions_impl(
    state: AppState,
    name: String,
    version: String,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let _auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let target_str = format!("{}@{}", name, version);

    // Query the audit log for package.promote actions targeting this package+version
    let entries: Vec<crate::db::AuditEntry> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE action = 'package.promote' AND target = ?1 ORDER BY created_at DESC",
    )
    .bind(&target_str)
    .fetch_all(&state.db)
    .await?;

    let promotions: Vec<Value> = entries
        .iter()
        .map(|e| {
            let details: Value = e
                .details_json
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or(json!({}));

            json!({
                "id": e.id,
                "package": name,
                "version": version,
                "from": details.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                "to": details.get("to").and_then(|v| v.as_str()).unwrap_or(""),
                "promoted_by": e.username,
                "promoted_at": e.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "package": name,
        "version": version,
        "promotions": promotions,
    })))
}
