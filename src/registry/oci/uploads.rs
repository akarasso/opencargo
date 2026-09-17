use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::db::kinds::Format;
use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::storage::StorageBackend;

use super::{param, paths, sha256_digest, OciRef};

fn chunk_path(upload_uuid: &str) -> String {
    format!("oci/_uploads/{upload_uuid}/data")
}

/// The hosted OCI repository `r` names, once the caller may write to it.
async fn writable_repo(
    state: &AppState,
    r: &OciRef,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<(Repository, AuthUser)> {
    let auth_user = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo = crate::registry::load_repo(&state.db, &r.repo).await?;
    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;
    Ok((repo, auth_user))
}

/// An upload id is only usable from the repository it was started in.
async fn owned_upload(state: &AppState, repo: &Repository, upload_uuid: &str) -> AppResult<()> {
    let upload = crate::db::oci::get_upload(&state.db, upload_uuid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("upload not found: {upload_uuid}")))?;
    if upload.repository_id != repo.id {
        return Err(AppError::Forbidden(
            "upload does not belong to this repository".to_string(),
        ));
    }
    Ok(())
}

pub async fn start_upload(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;

    let upload_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO oci_uploads (id, repository_id, name) VALUES (?1, ?2, ?3)")
        .bind(&upload_id)
        .bind(repo.id)
        .bind(&r.name)
        .execute(&state.db)
        .await?;

    let location = format!("/v2/{}/blobs/uploads/{}", r.image_name(), upload_id);
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

pub async fn upload_chunk(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let upload_uuid = param(&params, "uuid")?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;
    owned_upload(&state, &repo, upload_uuid).await?;

    // Append keeps a multi-chunk upload O(N) and yields the total for Range.
    let total_len = state
        .storage
        .append(&chunk_path(upload_uuid), body)
        .await?;

    let location = format!("/v2/{}/blobs/uploads/{}", r.image_name(), upload_uuid);
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
    let r = OciRef::parse(&params)?;
    let upload_uuid = param(&params, "uuid")?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;
    owned_upload(&state, &repo, upload_uuid).await?;

    let chunk_path = chunk_path(upload_uuid);
    let blob_data = assemble_blob(&state, &chunk_path, body).await;
    if blob_data.is_empty() {
        return Err(AppError::BadRequest("no blob data provided".to_string()));
    }
    let computed_digest = sha256_digest(&blob_data);
    if computed_digest != query.digest {
        return Err(AppError::BadRequest(format!(
            "digest mismatch: expected {}, computed {}",
            query.digest, computed_digest
        )));
    }

    state
        .storage
        .put(&paths::blob_path(&repo.name, &query.digest), blob_data.clone())
        .await?;
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
    sqlx::query("DELETE FROM oci_uploads WHERE id = ?1")
        .bind(upload_uuid)
        .execute(&state.db)
        .await?;
    let _ = state.storage.delete(&chunk_path).await;
    info!(digest = %query.digest, size = blob_data.len(), image = %r.image_name(), "OCI blob uploaded");

    Ok((
        StatusCode::CREATED,
        [
            ("Docker-Content-Digest", query.digest.clone()),
            ("Content-Length", "0".to_string()),
            ("Location", format!("/v2/{}/blobs/{}", r.image_name(), query.digest)),
        ],
    )
        .into_response())
}

/// The chunks PATCHed so far followed by the PUT body, whichever exist.
async fn assemble_blob(state: &AppState, chunk_path: &str, body: Bytes) -> Bytes {
    match state.storage.get(chunk_path).await {
        Ok(existing) if body.is_empty() => existing,
        Ok(existing) => {
            let mut combined = existing.to_vec();
            combined.extend_from_slice(&body);
            Bytes::from(combined)
        }
        Err(_) => body,
    }
}
