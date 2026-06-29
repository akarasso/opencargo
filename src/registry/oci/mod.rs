use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::db::oci::OciTag;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::storage::StorageBackend;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the sha256 digest of data and return the hex-encoded string.
fn sha256_digest(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Check whether a reference looks like a digest (e.g., "sha256:abc123...").
fn is_digest(reference: &str) -> bool {
    reference.starts_with("sha256:")
}

/// Build the image name from path params (repo/name).
fn extract_image_name(params: &HashMap<String, String>) -> String {
    let repo = params.get("repo").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    format!("{}/{}", repo, name)
}

// ---------------------------------------------------------------------------
// GET /v2/ — API Version Check
// ---------------------------------------------------------------------------

pub async fn api_version_check() -> impl IntoResponse {
    // The auth middleware has already handled authentication:
    // - If anonymous_read is false and no credentials are provided, the middleware
    //   returns 401 with Www-Authenticate header before we reach this handler.
    // - If we reach here, the request is either authenticated or anonymous read is allowed.
    (
        StatusCode::OK,
        [("Docker-Distribution-Api-Version", "registry/2.0")],
        Json(json!({})),
    )
}

// ---------------------------------------------------------------------------
// HEAD /v2/{repo}/{name}/blobs/{digest} — Check if blob exists
// ---------------------------------------------------------------------------

pub async fn head_blob(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<Response> {
    let image_name = extract_image_name(&params);
    let digest = params
        .get("digest")
        .ok_or_else(|| AppError::BadRequest("missing digest".to_string()))?;

    // Look up the repository for this image
    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    // Check if blob exists in DB
    let blob = crate::db::oci::get_blob(&state.db, repo.id, digest).await?;

    match blob {
        Some(b) => {
            let content_type = b
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_string());

            Ok((
                StatusCode::OK,
                [
                    ("Docker-Content-Digest", digest.to_string()),
                    ("Content-Type", content_type),
                    ("Content-Length", b.size.to_string()),
                ],
            )
                .into_response())
        }
        None => Err(AppError::NotFound(format!(
            "blob not found: {} in {}",
            digest, image_name
        ))),
    }
}

// ---------------------------------------------------------------------------
// GET /v2/{repo}/{name}/blobs/{digest} — Download blob
// ---------------------------------------------------------------------------

pub async fn get_blob(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<Response> {
    let image_name = extract_image_name(&params);
    let digest = params
        .get("digest")
        .ok_or_else(|| AppError::BadRequest("missing digest".to_string()))?;

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    // Check if blob exists in DB
    let blob = crate::db::oci::get_blob(&state.db, repo.id, digest).await?;

    let blob = blob.ok_or_else(|| {
        AppError::NotFound(format!("blob not found: {} in {}", digest, image_name))
    })?;

    // Read blob from storage
    let hex_digest = digest
        .strip_prefix("sha256:")
        .unwrap_or(digest);
    let storage_path = format!("oci/{}/blobs/sha256/{}", image_name, hex_digest);

    let data = state.storage.get(&storage_path).await?;
    let content_type = blob
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Ok((
        StatusCode::OK,
        [
            ("Docker-Content-Digest", digest.to_string()),
            ("Content-Type", content_type),
            ("Content-Length", data.len().to_string()),
        ],
        data,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// DELETE /v2/{repo}/{name}/blobs/{digest} — Delete blob
// ---------------------------------------------------------------------------

pub async fn delete_blob(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let image_name = extract_image_name(&params);
    let digest = params
        .get("digest")
        .ok_or_else(|| AppError::BadRequest("missing digest".to_string()))?;

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    // Refuse to delete a blob still referenced by a manifest — that would break
    // a live image. (Only manifests pushed since migration 012 are tracked.)
    let refs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2",
    )
    .bind(repo.id)
    .bind(digest)
    .fetch_one(&state.db)
    .await?;
    if refs > 0 {
        return Err(AppError::Conflict(format!(
            "blob {digest} is still referenced by {refs} manifest(s); delete those manifests first"
        )));
    }

    // Delete from DB
    let result = sqlx::query(
        "DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2",
    )
    .bind(repo.id)
    .bind(digest)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "blob not found: {} in {}",
            digest, image_name
        )));
    }

    // Delete from storage
    let hex_digest = digest.strip_prefix("sha256:").unwrap_or(digest);
    let storage_path = format!("oci/{}/blobs/sha256/{}", image_name, hex_digest);
    let _ = state.storage.delete(&storage_path).await;

    Ok(StatusCode::ACCEPTED.into_response())
}

// ---------------------------------------------------------------------------
// POST /v2/{repo}/{name}/blobs/uploads/ — Initiate blob upload
// ---------------------------------------------------------------------------

pub async fn start_upload(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only push to hosted repositories".to_string(),
        ));
    }
    crate::registry::ensure_format(&repo, "oci")?;

    // Create upload record
    let upload_id = uuid::Uuid::new_v4().to_string();
    let name = params.get("name").cloned().unwrap_or_default();

    sqlx::query(
        "INSERT INTO oci_uploads (id, repository_id, name) VALUES (?1, ?2, ?3)",
    )
    .bind(&upload_id)
    .bind(repo.id)
    .bind(&name)
    .execute(&state.db)
    .await?;

    let location = format!("/v2/{}/{}/blobs/uploads/{}", repo_name, name, upload_id);

    Ok((
        StatusCode::ACCEPTED,
        [
            ("Location", location),
            ("Docker-Upload-UUID", upload_id),
            ("Content-Length", "0".to_string()),
        ],
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// PATCH /v2/{repo}/{name}/blobs/uploads/{uuid} — Upload blob chunk
// ---------------------------------------------------------------------------

pub async fn upload_chunk(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<Response> {
    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let name = params.get("name").cloned().unwrap_or_default();
    let upload_uuid = params
        .get("uuid")
        .ok_or_else(|| AppError::BadRequest("missing upload uuid".to_string()))?;

    // Authn + authz: this path was previously unauthenticated, allowing anyone
    // with a valid upload UUID to write into any repo. Require an authenticated
    // user with write permission on the hosted repo named in the URL.
    let auth_user = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;
    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only push to hosted repositories".to_string(),
        ));
    }
    if !check_repo_permission(&state.db, auth_user.user_id, &auth_user.role, repo.id, "write").await
    {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    // Verify upload exists AND belongs to this repository.
    let upload = crate::db::oci::get_upload(&state.db, upload_uuid).await?;
    let upload = upload.ok_or_else(|| {
        AppError::NotFound(format!("upload not found: {upload_uuid}"))
    })?;
    if upload.repository_id != repo.id {
        return Err(AppError::Forbidden(
            "upload does not belong to this repository".to_string(),
        ));
    }

    // Accumulate the chunk in a temporary storage location. We append directly
    // instead of read-modify-write so a multi-chunk upload stays O(N) overall
    // rather than O(N²); `append` returns the new total size for the Range header.
    let chunk_path = format!("oci/_uploads/{}/{}", upload_uuid, "data");
    let total_len = state.storage.append(&chunk_path, body).await?;

    let location = format!("/v2/{}/{}/blobs/uploads/{}", repo_name, name, upload_uuid);

    Ok((
        StatusCode::ACCEPTED,
        [
            ("Location", location),
            ("Docker-Upload-UUID", upload_uuid.to_string()),
            ("Content-Length", "0".to_string()),
            ("Range", format!("0-{}", total_len.saturating_sub(1))),
        ],
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// PUT /v2/{repo}/{name}/blobs/uploads/{uuid}?digest={digest} — Complete upload
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CompleteUploadQuery {
    digest: String,
}

pub async fn complete_upload(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<CompleteUploadQuery>,
    auth: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<Response> {
    let image_name = extract_image_name(&params);
    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let upload_uuid = params
        .get("uuid")
        .ok_or_else(|| AppError::BadRequest("missing upload uuid".to_string()))?;

    // Authn + authz: require an authenticated user with write permission on the
    // hosted repo named in the URL (this path was previously unauthenticated).
    let auth_user = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;
    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only push to hosted repositories".to_string(),
        ));
    }
    if !check_repo_permission(&state.db, auth_user.user_id, &auth_user.role, repo.id, "write").await
    {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    // Verify upload exists AND belongs to this repository.
    let upload = crate::db::oci::get_upload(&state.db, upload_uuid).await?;
    let upload = upload.ok_or_else(|| {
        AppError::NotFound(format!("upload not found: {upload_uuid}"))
    })?;
    if upload.repository_id != repo.id {
        return Err(AppError::Forbidden(
            "upload does not belong to this repository".to_string(),
        ));
    }

    // Get the blob data: either from the PUT body (monolithic) or from chunked upload data
    let chunk_path = format!("oci/_uploads/{}/{}", upload_uuid, "data");
    let blob_data = if !body.is_empty() {
        // Check if there was previous chunk data
        let existing = state.storage.get(&chunk_path).await;
        match existing {
            Ok(existing_data) => {
                let mut combined = existing_data.to_vec();
                combined.extend_from_slice(&body);
                Bytes::from(combined)
            }
            Err(_) => body,
        }
    } else {
        // Try to read from chunked upload storage
        state.storage.get(&chunk_path).await.unwrap_or(Bytes::new())
    };

    if blob_data.is_empty() {
        return Err(AppError::BadRequest("no blob data provided".to_string()));
    }

    // Verify digest
    let computed_digest = sha256_digest(&blob_data);
    if computed_digest != query.digest {
        return Err(AppError::BadRequest(format!(
            "digest mismatch: expected {}, computed {}",
            query.digest, computed_digest
        )));
    }

    // Store the blob
    let hex_digest = query
        .digest
        .strip_prefix("sha256:")
        .unwrap_or(&query.digest);
    let storage_path = format!("oci/{}/blobs/sha256/{}", image_name, hex_digest);
    state
        .storage
        .put(&storage_path, blob_data.clone())
        .await?;

    // Insert blob record in DB
    sqlx::query(
        "INSERT OR IGNORE INTO oci_blobs (repository_id, digest, size, content_type)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(repo.id)
    .bind(&query.digest)
    .bind(blob_data.len() as i64)
    .bind("application/octet-stream")
    .execute(&state.db)
    .await?;

    // Clean up upload record and temp storage
    sqlx::query("DELETE FROM oci_uploads WHERE id = ?1")
        .bind(upload_uuid)
        .execute(&state.db)
        .await?;
    let _ = state.storage.delete(&chunk_path).await;

    info!(
        digest = %query.digest,
        size = blob_data.len(),
        image = %image_name,
        "OCI blob uploaded"
    );

    Ok((
        StatusCode::CREATED,
        [
            ("Docker-Content-Digest", query.digest.clone()),
            ("Content-Length", "0".to_string()),
            (
                "Location",
                format!("/v2/{}/blobs/{}", image_name, query.digest),
            ),
        ],
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /v2/{repo}/{name}/manifests/{reference} — Get manifest
// ---------------------------------------------------------------------------

pub async fn get_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<Response> {
    let image_name = extract_image_name(&params);
    let reference = params
        .get("reference")
        .ok_or_else(|| AppError::BadRequest("missing reference".to_string()))?;

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let name = params.get("name").cloned().unwrap_or_default();

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    // Resolve the digest: if reference is a tag, look up the digest
    let digest = if is_digest(reference) {
        reference.clone()
    } else {
        // Look up tag
        let tag: Option<OciTag> = sqlx::query_as(
            "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND tag = ?3",
        )
        .bind(repo.id)
        .bind(&name)
        .bind(reference)
        .fetch_optional(&state.db)
        .await?;

        tag.ok_or_else(|| {
            AppError::NotFound(format!(
                "manifest not found: {}:{} in {}",
                name, reference, repo_name
            ))
        })?
        .manifest_digest
    };

    // Fetch manifest from DB
    let manifest = crate::db::oci::get_manifest(&state.db, repo.id, &name, &digest).await?;

    let manifest = manifest.ok_or_else(|| {
        AppError::NotFound(format!(
            "manifest not found: {}@{} in {}",
            name, digest, repo_name
        ))
    })?;

    // Read manifest from storage
    let hex_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let storage_path = format!(
        "oci/{}/manifests/{}/sha256/{}",
        image_name, name, hex_digest
    );
    let data = state.storage.get(&storage_path).await?;

    Ok((
        StatusCode::OK,
        [
            ("Docker-Content-Digest", digest),
            ("Content-Type", manifest.content_type),
            ("Content-Length", data.len().to_string()),
        ],
        data,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// HEAD /v2/{repo}/{name}/manifests/{reference} — Check manifest exists
// ---------------------------------------------------------------------------

pub async fn head_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<Response> {
    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let name = params.get("name").cloned().unwrap_or_default();
    let reference = params
        .get("reference")
        .ok_or_else(|| AppError::BadRequest("missing reference".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    // Resolve digest
    let digest = if is_digest(reference) {
        reference.clone()
    } else {
        let tag: Option<OciTag> = sqlx::query_as(
            "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND tag = ?3",
        )
        .bind(repo.id)
        .bind(&name)
        .bind(reference)
        .fetch_optional(&state.db)
        .await?;

        tag.ok_or_else(|| {
            AppError::NotFound(format!(
                "manifest not found: {}:{} in {}",
                name, reference, repo_name
            ))
        })?
        .manifest_digest
    };

    let manifest = crate::db::oci::get_manifest(&state.db, repo.id, &name, &digest).await?;

    let manifest = manifest.ok_or_else(|| {
        AppError::NotFound(format!(
            "manifest not found: {}@{} in {}",
            name, digest, repo_name
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Docker-Content-Digest", digest),
            ("Content-Type", manifest.content_type),
            ("Content-Length", manifest.size.to_string()),
        ],
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// PUT /v2/{repo}/{name}/manifests/{reference} — Push manifest
// ---------------------------------------------------------------------------

/// Extract the blob digests an image manifest references: its config blob and
/// every layer. Best-effort — a manifest list / unknown shape yields none.
fn extract_blob_digests(manifest_json: &[u8]) -> Vec<String> {
    let mut digests = Vec::new();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest_json) {
        if let Some(d) = v
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(|d| d.as_str())
        {
            digests.push(d.to_string());
        }
        if let Some(layers) = v.get("layers").and_then(|l| l.as_array()) {
            for layer in layers {
                if let Some(d) = layer.get("digest").and_then(|d| d.as_str()) {
                    digests.push(d.to_string());
                }
            }
        }
    }
    digests
}

pub async fn put_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let image_name = extract_image_name(&params);
    let reference = params
        .get("reference")
        .ok_or_else(|| AppError::BadRequest("missing reference".to_string()))?
        .clone();

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?
        .clone();
    let name = params.get("name").cloned().unwrap_or_default();

    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only push to hosted repositories".to_string(),
        ));
    }

    // Read the manifest body
    let body = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();

    // Compute digest
    let digest = sha256_digest(&body);

    // Store manifest
    let hex_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let storage_path = format!(
        "oci/{}/manifests/{}/sha256/{}",
        image_name, name, hex_digest
    );
    state
        .storage
        .put(&storage_path, body.clone())
        .await?;

    // Insert manifest record in DB
    sqlx::query(
        "INSERT OR REPLACE INTO oci_manifests (repository_id, name, digest, content_type, size)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(repo.id)
    .bind(&name)
    .bind(&digest)
    .bind(&content_type)
    .bind(body.len() as i64)
    .execute(&state.db)
    .await?;

    // Record which blobs this manifest references (for refcount-based GC).
    sqlx::query("DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2")
        .bind(repo.id)
        .bind(&digest)
        .execute(&state.db)
        .await?;
    for blob_digest in extract_blob_digests(&body) {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO oci_manifest_blobs (repository_id, manifest_digest, blob_digest)
             VALUES (?1, ?2, ?3)",
        )
        .bind(repo.id)
        .bind(&digest)
        .bind(&blob_digest)
        .execute(&state.db)
        .await;
    }

    // If reference is a tag (not a digest), create/update the tag mapping
    if !is_digest(&reference) {
        sqlx::query(
            "INSERT INTO oci_tags (repository_id, name, tag, manifest_digest)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repository_id, name, tag) DO UPDATE SET manifest_digest = excluded.manifest_digest",
        )
        .bind(repo.id)
        .bind(&name)
        .bind(&reference)
        .bind(&digest)
        .execute(&state.db)
        .await?;
    }

    info!(
        reference = %reference,
        digest = %digest,
        size = body.len(),
        image = %image_name,
        "OCI manifest pushed"
    );

    Ok((
        StatusCode::CREATED,
        [
            ("Docker-Content-Digest", digest.clone()),
            ("Content-Length", "0".to_string()),
            (
                "Location",
                format!("/v2/{}/manifests/{}", image_name, digest),
            ),
        ],
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// DELETE /v2/{repo}/{name}/manifests/{reference} — Delete manifest
// ---------------------------------------------------------------------------

pub async fn delete_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let image_name = extract_image_name(&params);
    let reference = params
        .get("reference")
        .ok_or_else(|| AppError::BadRequest("missing reference".to_string()))?;

    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let name = params.get("name").cloned().unwrap_or_default();

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    // Resolve digest
    let digest = if is_digest(reference) {
        reference.clone()
    } else {
        let tag: Option<OciTag> = sqlx::query_as(
            "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND tag = ?3",
        )
        .bind(repo.id)
        .bind(&name)
        .bind(reference)
        .fetch_optional(&state.db)
        .await?;

        tag.ok_or_else(|| {
            AppError::NotFound(format!(
                "manifest not found: {}:{} in {}",
                name, reference, repo_name
            ))
        })?
        .manifest_digest
    };

    // Delete manifest from DB
    let result = sqlx::query(
        "DELETE FROM oci_manifests WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
    )
    .bind(repo.id)
    .bind(&name)
    .bind(&digest)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "manifest not found: {}@{} in {}",
            name, digest, repo_name
        )));
    }

    // Delete associated tags pointing to this digest
    sqlx::query(
        "DELETE FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND manifest_digest = ?3",
    )
    .bind(repo.id)
    .bind(&name)
    .bind(&digest)
    .execute(&state.db)
    .await?;

    // GC: drop this manifest's blob links, then delete any blob no longer
    // referenced by any manifest in this repo (DB row + stored file).
    let blob_digests: Vec<String> = sqlx::query_scalar(
        "SELECT blob_digest FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2",
    )
    .bind(repo.id)
    .bind(&digest)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let _ = sqlx::query(
        "DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2",
    )
    .bind(repo.id)
    .bind(&digest)
    .execute(&state.db)
    .await;
    for blob_digest in blob_digests {
        let still: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2",
        )
        .bind(repo.id)
        .bind(&blob_digest)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        if still == 0 {
            let _ = sqlx::query("DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2")
                .bind(repo.id)
                .bind(&blob_digest)
                .execute(&state.db)
                .await;
            let hex = blob_digest.strip_prefix("sha256:").unwrap_or(&blob_digest);
            let _ = state
                .storage
                .delete(&format!("oci/{}/blobs/sha256/{}", image_name, hex))
                .await;
        }
    }

    // Delete from storage
    let hex_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let storage_path = format!(
        "oci/{}/manifests/{}/sha256/{}",
        image_name, name, hex_digest
    );
    let _ = state.storage.delete(&storage_path).await;

    Ok(StatusCode::ACCEPTED.into_response())
}

// ---------------------------------------------------------------------------
// GET /v2/{repo}/{name}/tags/list — List tags
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ListTagsQuery {
    #[allow(dead_code)]
    n: Option<i64>,
    #[allow(dead_code)]
    last: Option<String>,
}

pub async fn list_tags(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<ListTagsQuery>,
    auth: Option<axum::Extension<crate::auth::middleware::AuthUser>>,
) -> AppResult<Response> {
    let repo_name = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("missing repository".to_string()))?;
    let name = params.get("name").cloned().unwrap_or_default();

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    let limit = query.n.unwrap_or(100).min(10000);

    let tags: Vec<OciTag> = match &query.last {
        Some(last) => {
            sqlx::query_as(
                "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND tag > ?3 ORDER BY tag LIMIT ?4",
            )
            .bind(repo.id)
            .bind(&name)
            .bind(last)
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 ORDER BY tag LIMIT ?3",
            )
            .bind(repo.id)
            .bind(&name)
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
    };

    let tag_names: Vec<String> = tags.iter().map(|t| t.tag.clone()).collect();
    let full_name = format!("{}/{}", repo_name, name);

    Ok(Json(json!({
        "name": full_name,
        "tags": tag_names,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Row types for OCI tables
// ---------------------------------------------------------------------------

// OCI row types (OciBlob/OciManifest/OciTag/OciUpload) moved to the DAL:
// crate::db::oci (imported at the top of this file).
