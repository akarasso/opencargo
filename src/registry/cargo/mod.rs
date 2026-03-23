use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::storage::StorageBackend;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the sparse-index prefix path for a crate name.
///
/// - 1 char  → `1`
/// - 2 chars → `2`
/// - 3 chars → `3/{first_char}`
/// - 4+ chars → `{first_two}/{next_two}`
pub fn compute_prefix(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", &lower[..1]),
        _ => format!("{}/{}", &lower[..2], &lower[2..4]),
    }
}

/// Build one JSON line for the sparse index from a version row.
fn build_index_line(
    crate_name: &str,
    version: &crate::db::Version,
) -> Result<String, serde_json::Error> {
    // Parse the stored metadata to extract deps, features, and other fields.
    let meta: Value = serde_json::from_str(&version.metadata_json).unwrap_or(json!({}));

    let deps = meta.get("deps").cloned().unwrap_or(json!([]));
    let features = meta.get("features").cloned().unwrap_or(json!({}));
    let features2 = meta.get("features2").cloned();
    let links = meta.get("links").cloned();

    let cksum = version
        .checksum_sha256
        .clone()
        .unwrap_or_default();

    let mut line = json!({
        "name": crate_name,
        "vers": version.version,
        "deps": deps,
        "cksum": cksum,
        "features": features,
        "yanked": version.yanked != 0,
    });

    if let Some(f2) = features2 {
        line.as_object_mut().unwrap().insert("features2".to_string(), f2);
    }
    if let Some(l) = links {
        line.as_object_mut().unwrap().insert("links".to_string(), l);
    }

    serde_json::to_string(&line)
}

// ---------------------------------------------------------------------------
// config.json — GET /{repo}/index/config.json
// ---------------------------------------------------------------------------

pub async fn config_json(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
) -> AppResult<impl IntoResponse> {
    // Verify the repository exists
    let _repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let config = json!({
        "dl": format!("{}/{}/api/v1/crates", state.base_url, repo_name),
        "api": format!("{}/{}", state.base_url, repo_name),
    });

    Ok(Json(config))
}

// ---------------------------------------------------------------------------
// Index entry — GET /{repo}/index/{prefix...}/{name}
// ---------------------------------------------------------------------------

/// Handler for all index prefix routes. The crate name is always the last
/// path segment captured as `{name}`.
pub async fn get_index_entry(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let crate_name = params.get("name").ok_or_else(|| {
        AppError::BadRequest("missing crate name".to_string())
    })?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, crate_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("crate not found: {crate_name}")))?;

    let versions = crate::db::get_versions(&state.db, package.id).await?;

    if versions.is_empty() {
        return Err(AppError::NotFound(format!(
            "no versions found for crate: {crate_name}"
        )));
    }

    // Build one JSON line per version, separated by newlines
    let mut lines = Vec::new();
    for v in &versions {
        let line = build_index_line(crate_name, v)?;
        lines.push(line);
    }

    let body = lines.join("\n");

    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json".to_string(),
        )],
        body,
    ))
}

// ---------------------------------------------------------------------------
// Publish — PUT /{repo}/api/v1/crates/new
// ---------------------------------------------------------------------------

/// Cargo publish metadata (the JSON portion of the PUT body).
#[derive(Debug, Deserialize, Serialize)]
struct CargoPublishMeta {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Value,
    #[serde(default)]
    features: Value,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    readme: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    links: Option<String>,
    #[serde(default)]
    features2: Option<Value>,
    // Catch-all for extra fields
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

pub async fn publish_crate(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth_user: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;

    // Validate repo exists and is hosted
    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only publish to hosted repositories".to_string(),
        ));
    }

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, user.user_id, &user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    // Parse the binary publish format:
    //   4 bytes LE u32 — JSON metadata length
    //   N bytes        — JSON metadata
    //   4 bytes LE u32 — crate file length
    //   M bytes        — .crate file (gzip'd tar)
    let data = body.as_ref();

    if data.len() < 4 {
        return Err(AppError::BadRequest(
            "request body too short: missing JSON length".to_string(),
        ));
    }

    let json_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let json_start = 4;
    let json_end = json_start + json_len;

    if data.len() < json_end + 4 {
        return Err(AppError::BadRequest(
            "request body too short: missing crate data".to_string(),
        ));
    }

    let meta: CargoPublishMeta = serde_json::from_slice(&data[json_start..json_end])
        .map_err(|e| AppError::BadRequest(format!("invalid publish metadata JSON: {e}")))?;

    let crate_len =
        u32::from_le_bytes([data[json_end], data[json_end + 1], data[json_end + 2], data[json_end + 3]])
            as usize;
    let crate_start = json_end + 4;
    let crate_end = crate_start + crate_len;

    if data.len() < crate_end {
        return Err(AppError::BadRequest(
            "request body too short: crate file truncated".to_string(),
        ));
    }

    let crate_data = &data[crate_start..crate_end];

    // Compute SHA-256 checksum of the .crate file
    let sha256_hex = hex::encode(sha2::Sha256::digest(crate_data));

    let crate_name = &meta.name;
    let version_str = &meta.vers;

    // Get or create the package
    let package = match crate::db::get_package(&state.db, repo.id, crate_name).await? {
        Some(p) => p,
        None => {
            let _id = crate::db::create_package(
                &state.db,
                repo.id,
                crate_name,
                meta.description.as_deref(),
            )
            .await?;
            crate::db::get_package(&state.db, repo.id, crate_name)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!("failed to create package: {crate_name}"))
                })?
        }
    };

    // Check if version already exists
    if crate::db::get_version(&state.db, package.id, version_str)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "version {version_str} already exists for {crate_name}"
        )));
    }

    // Storage path for the .crate file
    let storage_path = format!(
        "cargo/{}/{}/{}-{}.crate",
        repo_name, crate_name, crate_name, version_str
    );

    // Store the .crate file
    state
        .storage
        .put(&storage_path, Bytes::from(crate_data.to_vec()))
        .await?;

    // Build the metadata JSON to store in the DB (the full cargo metadata)
    let metadata_json = serde_json::to_string(&meta)?;
    let size = crate_data.len() as i64;

    // Insert version in DB
    let _version_id = crate::db::create_version(
        &state.db,
        package.id,
        version_str,
        &metadata_json,
        None, // no SHA-1 for cargo
        Some(&sha256_hex),
        None, // no SRI integrity for cargo
        size,
        &storage_path,
    )
    .await?;

    // Extract dependencies from cargo metadata and store them
    if let Some(deps_array) = meta.deps.as_array() {
        for dep in deps_array {
            let dep_name = dep.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let dep_version_req = dep.get("version_req").and_then(|v| v.as_str()).unwrap_or("*");
            let dep_kind = dep.get("kind").and_then(|k| k.as_str());
            let dep_type = match dep_kind {
                Some("dev") => "dev",
                Some("build") => "build",
                _ => "normal",
            };
            if !dep_name.is_empty() {
                let _ = crate::db::insert_dependency(
                    &state.db,
                    package.id,
                    _version_id,
                    dep_name,
                    dep_version_req,
                    dep_type,
                )
                .await;
            }
        }
    }

    // Dispatch webhook for package.published
    state.webhook_dispatcher.dispatch("package.published", &serde_json::json!({
        "package": crate_name,
        "version": version_str,
        "repository": repo_name,
        "published_by": user.username,
    })).await;

    // Vulnerability scan
    if state.vuln_scan_config.block_on_critical {
        // Blocking scan: check for critical vulns before accepting
        let scan_result = state
            .vuln_scanner
            .scan_version(&state.db, _version_id, &metadata_json, "crates.io")
            .await;
        if let Ok(ref result) = scan_result {
            if result.status == "critical" {
                return Err(AppError::BadRequest(
                    "publish blocked: critical vulnerabilities found in dependencies".to_string(),
                ));
            }
        }
    } else {
        // Non-blocking background scan
        let scanner = state.vuln_scanner.clone();
        let db = state.db.clone();
        let meta_json = metadata_json.clone();
        let vid = _version_id;
        tokio::spawn(async move {
            if let Err(e) = scanner.scan_version(&db, vid, &meta_json, "crates.io").await {
                tracing::warn!(error = %e, "Background vulnerability scan failed");
            }
        });
    }

    info!(
        crate_name = %crate_name,
        version = %version_str,
        size = size,
        repo = %repo_name,
        "Cargo crate published"
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "warnings": {
                "invalid_categories": [],
                "invalid_badges": [],
                "other": []
            }
        })),
    ))
}

// ---------------------------------------------------------------------------
// Download — GET /{repo}/api/v1/crates/{name}/{version}/download
// ---------------------------------------------------------------------------

pub async fn download_crate(
    State(state): State<AppState>,
    Path((repo_name, crate_name, version_str)): Path<(String, String, String)>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, &crate_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("crate not found: {crate_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {crate_name}@{version_str}"
            ))
        })?;

    // Record download
    let _ = crate::db::record_download(&state.db, version.id).await;

    // Read from storage
    let data = state.storage.get(&version.tarball_path).await?;
    let filename = format!("{}-{}.crate", crate_name, version_str)
        .replace('"', "")
        .replace('\n', "")
        .replace('\r', "");

    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/x-tar".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        data,
    ))
}

// ---------------------------------------------------------------------------
// Yank — DELETE /{repo}/api/v1/crates/{name}/{version}/yank
// ---------------------------------------------------------------------------

pub async fn yank(
    State(state): State<AppState>,
    Path((repo_name, crate_name, version_str)): Path<(String, String, String)>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;

    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, user.user_id, &user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only yank on hosted repositories".to_string(),
        ));
    }

    let package = crate::db::get_package(&state.db, repo.id, &crate_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("crate not found: {crate_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {crate_name}@{version_str}"
            ))
        })?;

    crate::db::set_yanked(&state.db, version.id, true).await?;

    info!(
        crate_name = %crate_name,
        version = %version_str,
        repo = %repo_name,
        "Cargo crate version yanked"
    );

    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Unyank — PUT /{repo}/api/v1/crates/{name}/{version}/unyank
// ---------------------------------------------------------------------------

pub async fn unyank(
    State(state): State<AppState>,
    Path((repo_name, crate_name, version_str)): Path<(String, String, String)>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;

    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, user.user_id, &user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only unyank on hosted repositories".to_string(),
        ));
    }

    let package = crate::db::get_package(&state.db, repo.id, &crate_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("crate not found: {crate_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {crate_name}@{version_str}"
            ))
        })?;

    crate::db::set_yanked(&state.db, version.id, false).await?;

    info!(
        crate_name = %crate_name,
        version = %version_str,
        repo = %repo_name,
        "Cargo crate version unyanked"
    );

    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Hex helper (same pattern as npm module)
// ---------------------------------------------------------------------------

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
