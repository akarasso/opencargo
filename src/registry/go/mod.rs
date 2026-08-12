use std::collections::HashMap;
use std::io::Read as _;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::storage::StorageBackend;

// ---------------------------------------------------------------------------
// List versions — GET /{repo}/{module}/@v/list
// ---------------------------------------------------------------------------

pub async fn list_versions(
    State(state): State<AppState>,
    Path((repo_name, module_name)): Path<(String, String)>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    let package = match crate::db::get_package(&state.db, repo.id, &module_name).await? {
        Some(p) => p,
        None => {
            // Return empty list if module not found
            return Ok((StatusCode::OK, "".to_string()).into_response());
        }
    };

    let versions = crate::db::get_versions(&state.db, package.id).await?;

    let version_list: String = versions
        .iter()
        .map(|v| v.version.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8".to_string(),
        )],
        version_list,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Latest version — GET /{repo}/{module}/@latest
// ---------------------------------------------------------------------------

/// GOPROXY `@latest`: JSON info about the most recently published version.
/// Reached through the wildcard Go dispatcher in `server.rs` (the module path
/// may span multiple URL segments).
pub async fn latest_version(
    State(state): State<AppState>,
    Path((repo_name, module_name)): Path<(String, String)>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    let package = crate::db::get_package(&state.db, repo.id, &module_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("module not found: {module_name}")))?;

    let versions = crate::db::get_versions(&state.db, package.id).await?;
    // get_versions is ordered by insertion (id): the last row is the most
    // recently published version.
    let latest = versions
        .last()
        .ok_or_else(|| AppError::NotFound(format!("no versions for module: {module_name}")))?;

    Ok(Json(json!({
        "Version": latest.version,
        "Time": latest.published_at,
    })))
}

// ---------------------------------------------------------------------------
// Version info — GET /{repo}/{module}/@v/{version}.info
// ---------------------------------------------------------------------------

pub async fn version_info(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let module_name = params.get("module").ok_or_else(|| {
        AppError::BadRequest("missing module".to_string())
    })?;
    let version_raw = params.get("version").ok_or_else(|| {
        AppError::BadRequest("missing version".to_string())
    })?;
    // Strip ".info" suffix if present (from route matching)
    let version_str = version_raw.strip_suffix(".info").unwrap_or(version_raw);

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, module_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("module not found: {module_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {module_name}@{version_str}"
            ))
        })?;

    // Try to parse stored info from metadata, or build a basic one
    let info = json!({
        "Version": version.version,
        "Time": version.published_at,
    });

    Ok(Json(info))
}

// ---------------------------------------------------------------------------
// Get go.mod — GET /{repo}/{module}/@v/{version}.mod
// ---------------------------------------------------------------------------

pub async fn get_mod(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let module_name = params.get("module").ok_or_else(|| {
        AppError::BadRequest("missing module".to_string())
    })?;
    let version_raw = params.get("version").ok_or_else(|| {
        AppError::BadRequest("missing version".to_string())
    })?;
    let version_str = version_raw.strip_suffix(".mod").unwrap_or(version_raw);

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, module_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("module not found: {module_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {module_name}@{version_str}"
            ))
        })?;

    // The go.mod content is stored in metadata_json
    let go_mod = version.metadata_json.clone();

    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8".to_string(),
        )],
        go_mod,
    ))
}

// ---------------------------------------------------------------------------
// Get zip — GET /{repo}/{module}/@v/{version}.zip
// ---------------------------------------------------------------------------

pub async fn get_zip(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let module_name = params.get("module").ok_or_else(|| {
        AppError::BadRequest("missing module".to_string())
    })?;
    let version_raw = params.get("version").ok_or_else(|| {
        AppError::BadRequest("missing version".to_string())
    })?;
    let version_str = version_raw.strip_suffix(".zip").unwrap_or(version_raw);

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, module_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("module not found: {module_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version not found: {module_name}@{version_str}"
            ))
        })?;

    // Record download
    let _ = crate::db::record_download(&state.db, version.id).await;

    // Read from storage
    let data = state.storage.get(&version.tarball_path).await?;
    let filename = format!("{}.zip", version_str);

    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
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
// Publish — PUT /{repo}/{module}/@v/{version}
// ---------------------------------------------------------------------------

pub async fn publish_module(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let module_name = params.get("module").ok_or_else(|| {
        AppError::BadRequest("missing module".to_string())
    })?;
    let version_str = params.get("version").ok_or_else(|| {
        AppError::BadRequest("missing version".to_string())
    })?;

    // Reject hostile module paths ('..' or empty segments, forbidden chars)
    // and versions before touching DB or storage — the module path is
    // interpolated into the storage path and may span multiple segments.
    crate::registry::validate_package_name("go", module_name)?;
    crate::registry::validate_version(version_str)?;

    // Validate repo exists and is hosted
    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, "go")?;

    // Read the zip body
    let zip_data = axum::body::to_bytes(request.into_body(), 100 * 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;

    // Extract go.mod from the zip file. The synchronous zip inflation runs on
    // a blocking thread so a large archive (body cap is 100 MiB) does not
    // stall a tokio worker; the `Bytes` clone is a cheap refcount bump.
    let go_mod_content = {
        let zip_data = zip_data.clone();
        let module_name = module_name.clone();
        let version_str = version_str.clone();
        tokio::task::spawn_blocking(move || {
            extract_go_mod_from_zip(&zip_data, &module_name, &version_str)
        })
        .await
        .map_err(|e| AppError::Internal(format!("go.mod extraction task failed: {e}")))??
    };

    // Get or create the package
    let package = match crate::db::get_package(&state.db, repo.id, module_name).await? {
        Some(p) => p,
        None => {
            let _id = crate::db::create_package(
                &state.db,
                repo.id,
                module_name,
                Some(&format!("Go module {module_name}")),
            )
            .await?;
            crate::db::get_package(&state.db, repo.id, module_name)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!("failed to create package: {module_name}"))
                })?
        }
    };

    // Check if version already exists
    if crate::db::get_version(&state.db, package.id, version_str)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "version {version_str} already exists for {module_name}"
        )));
    }

    // Storage path for the zip file
    let storage_path = format!(
        "go/{}/{}/{}.zip",
        repo_name, module_name, version_str
    );

    let size = zip_data.len() as i64;

    // Store the zip file
    state
        .storage
        .put(&storage_path, zip_data)
        .await?;

    // Store go.mod content in metadata_json, zip path in tarball_path
    let version_id = crate::db::create_version(
        &state.db,
        package.id,
        version_str,
        &go_mod_content,
        None,
        None,
        None,
        size,
        &storage_path,
    )
    .await?;

    // Shared post-publish side effects: `package.published` webhook, real-time
    // event, vulnerability scan — same as npm/cargo. "Go" is the OSV.dev
    // ecosystem name (like "crates.io" for cargo).
    crate::registry::finalize_publish(
        &state,
        "Go",
        repo_name,
        module_name,
        version_str,
        Some(version_id),
        &go_mod_content,
        &auth_user.username,
    )
    .await?;

    info!(
        module = %module_name,
        version = %version_str,
        size = size,
        repo = %repo_name,
        "Go module published"
    );

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// Extract go.mod content from a zip archive.
/// Go module zips typically have go.mod at `{module}@{version}/go.mod`.
fn extract_go_mod_from_zip(
    zip_data: &[u8],
    _module_name: &str,
    _version: &str,
) -> Result<String, AppError> {
    let reader = std::io::Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| AppError::BadRequest(format!("invalid zip file: {e}")))?;

    // Look for go.mod in any path within the archive
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::BadRequest(format!("failed to read zip entry: {e}")))?;

        if file.name().ends_with("go.mod") {
            // Bound the decompressed read to defend against zip bombs: a go.mod
            // entry could declare a tiny compressed size yet inflate to GiBs.
            const MAX_GO_MOD_BYTES: u64 = 1024 * 1024; // 1 MiB is ample
            if file.size() > MAX_GO_MOD_BYTES {
                return Err(AppError::BadRequest(
                    "go.mod entry too large".to_string(),
                ));
            }
            let mut contents = String::new();
            file.by_ref()
                .take(MAX_GO_MOD_BYTES)
                .read_to_string(&mut contents)
                .map_err(|e| AppError::BadRequest(format!("failed to read go.mod: {e}")))?;
            return Ok(contents);
        }
    }

    // If no go.mod found, create a minimal one
    Ok(format!("module {_module_name}\n\ngo 1.21\n"))
}
