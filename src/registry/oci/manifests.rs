use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::oci::OciStore;
use crate::registry::cx;
use crate::registry::resolve::first_hit;
use crate::server::AppState;

use super::leaves::ManifestLeaf;
use super::{is_digest, param, parse_digest, paths, refs, respond, sha256_digest, OciRef};

const MAX_MANIFEST_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The digest a hosted reference names: itself, or the tag's target. A
/// reference that already is a digest names itself, which is why the store is
/// only ever asked about a tag.
pub(super) async fn resolve_hosted_digest(
    store: &dyn OciStore,
    repository_id: i64,
    name: &str,
    reference: &str,
) -> Result<Option<String>, StoreError> {
    if is_digest(reference) {
        return Ok(Some(reference.to_string()));
    }
    store.digest_for_ref(repository_id, name, reference).await
}

fn parse_reference(reference: &str) -> AppResult<String> {
    if is_digest(reference) {
        parse_digest(reference)
    } else {
        crate::domain::validate_oci_tag(reference)?;
        Ok(reference.to_string())
    }
}

pub async fn get_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    serve_manifest(&state, &params, auth.as_ref().map(|e| &e.0), false).await
}

pub async fn head_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    serve_manifest(&state, &params, auth.as_ref().map(|e| &e.0), true).await
}

async fn serve_manifest(
    state: &AppState,
    params: &HashMap<String, String>,
    auth: Option<&AuthUser>,
    head: bool,
) -> AppResult<Response> {
    let r = OciRef::parse(params)?;
    let reference = parse_reference(param(params, "reference")?)?;
    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;

    let leaf = ManifestLeaf {
        name: r.name,
        reference,
        head,
    };
    let payload = first_hit(&cx(state, auth, &repo), &repo, &leaf).await?;
    respond(state, payload).await
}

pub async fn put_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let reference = parse_reference(param(&params, "reference")?)?;

    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;

    let body = axum::body::to_bytes(request.into_body(), MAX_MANIFEST_BYTES)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE)
        .to_string();
    let digest = sha256_digest(&body);
    if is_digest(&reference) && reference != digest {
        return Err(AppError::BadRequest(format!(
            "manifest digest mismatch: reference '{reference}' != computed '{digest}'"
        )));
    }

    store_manifest(&state, &repo, &r, &reference, &digest, &content_type, &body).await?;
    // The "package" is the image name, the "version" the pushed reference;
    // OCI has no `versions` row, so no vulnerability scan applies.
    crate::registry::publish::finalize_publish(
        &state,
        Format::Oci,
        &r.repo,
        &r.name,
        &reference,
        None,
        &String::from_utf8_lossy(&body),
        &auth_user.username,
        crate::registry::publish::PreScan::default(),
    )
    .await?;
    info!(
        reference = %reference,
        digest = %digest,
        size = body.len(),
        image = %r.image_name(),
        "OCI manifest pushed"
    );

    Ok((
        StatusCode::CREATED,
        [
            ("Docker-Content-Digest", digest.clone()),
            ("Content-Length", "0".to_string()),
            (
                "Location",
                format!("/v2/{}/manifests/{}", r.image_name(), digest),
            ),
        ],
    )
        .into_response())
}

/// File first, then the manifest row, its blob links and, for a tag, the
/// tag mapping.
async fn store_manifest(
    state: &AppState,
    repo: &Repository,
    r: &OciRef,
    reference: &str,
    digest: &str,
    content_type: &str,
    body: &Bytes,
) -> AppResult<()> {
    state
        .storage
        .put(
            &paths::manifest_path(&r.image_name(), &r.name, digest),
            body.clone(),
        )
        .await?;
    sqlx::query(
        "INSERT OR REPLACE INTO oci_manifests (repository_id, name, digest, content_type, size)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(repo.id)
    .bind(&r.name)
    .bind(digest)
    .bind(content_type)
    .bind(body.len() as i64)
    .execute(&state.db)
    .await?;

    sqlx::query("DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2")
        .bind(repo.id)
        .bind(digest)
        .execute(&state.db)
        .await?;
    for blob_digest in refs::extract_refs(body) {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO oci_manifest_blobs (repository_id, manifest_digest, blob_digest)
             VALUES (?1, ?2, ?3)",
        )
        .bind(repo.id)
        .bind(digest)
        .bind(&blob_digest)
        .execute(&state.db)
        .await;
    }

    if !is_digest(reference) {
        sqlx::query(
            "INSERT INTO oci_tags (repository_id, name, tag, manifest_digest)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repository_id, name, tag) DO UPDATE SET manifest_digest = excluded.manifest_digest",
        )
        .bind(repo.id)
        .bind(&r.name)
        .bind(reference)
        .bind(digest)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

pub async fn delete_manifest(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let reference = parse_reference(param(&params, "reference")?)?;

    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;

    let digest = resolve_hosted_digest(state.oci.as_ref(), repo.id, &r.name, &reference)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "manifest not found: {}:{} in {}",
                r.name, reference, r.repo
            ))
        })?;
    let result = sqlx::query(
        "DELETE FROM oci_manifests WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
    )
    .bind(repo.id)
    .bind(&r.name)
    .bind(&digest)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "manifest not found: {}@{} in {}",
            r.name, digest, r.repo
        )));
    }
    sqlx::query(
        "DELETE FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND manifest_digest = ?3",
    )
    .bind(repo.id)
    .bind(&r.name)
    .bind(&digest)
    .execute(&state.db)
    .await?;

    gc_orphaned_blobs(&state, &repo, &digest).await;
    let _ = state
        .storage
        .delete(&paths::manifest_path(&r.image_name(), &r.name, &digest))
        .await;

    Ok(StatusCode::ACCEPTED.into_response())
}

/// Drop the manifest's blob links, then every blob no manifest of this
/// repository references any more (row and file).
async fn gc_orphaned_blobs(state: &AppState, repo: &Repository, digest: &str) {
    let blob_digests: Vec<String> = sqlx::query_scalar(
        "SELECT blob_digest FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2",
    )
    .bind(repo.id)
    .bind(digest)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let _ = sqlx::query(
        "DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2",
    )
    .bind(repo.id)
    .bind(digest)
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
            let _ = state
                .storage
                .delete(&paths::blob_path(&repo.name, &blob_digest))
                .await;
        }
    }
}
