use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tracing::info;

use crate::app::oci::{AppendChunk, CompleteUpload, Completion, OciWriteError};
use crate::auth::middleware::AuthUser;
use crate::domain::{layout, Format, Repository};
use crate::error::{AppError, AppResult};
use crate::server::AppState;

use super::{oci_error, param, parse_digest, OciRef};

/// The hosted OCI repository `r` names, once the caller may write to it.
async fn writable_repo(
    state: &AppState,
    r: &OciRef,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<(Repository, AuthUser)> {
    let auth_user = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, Some(&r.name), &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;
    Ok((repo, auth_user))
}

/// The incarnation prefix every new key of `repo` lies under.
pub(super) async fn repo_prefix(state: &AppState, repo: &Repository) -> AppResult<String> {
    let incarnation = state
        .repos
        .incarnation(repo.id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {}", repo.name)))?;
    Ok(layout::incarnation_prefix(&incarnation))
}

fn location(r: &OciRef, upload: &str) -> String {
    format!("/v2/{}/blobs/uploads/{}", r.image_name(), upload)
}

fn progress(r: &OciRef, upload: &str, received: u64, status: StatusCode) -> Response {
    (
        status,
        [
            ("Location", location(r, upload)),
            ("Docker-Upload-UUID", upload.to_string()),
            ("Content-Length", "0".to_string()),
            ("Range", format!("0-{}", received.saturating_sub(1))),
        ],
    )
        .into_response()
}

/// An unknown id and an id started in another repository are the same 404:
/// a foreign id reveals nothing.
fn unknown_upload(upload: &str) -> Response {
    oci_error(
        StatusCode::NOT_FOUND,
        "BLOB_UPLOAD_UNKNOWN",
        &format!("upload not found: {upload}"),
    )
}

/// `Content-Range: <start>-<end>`, the offset a chunk claims to start at.
fn chunk_start(headers: &HeaderMap) -> Result<Option<u64>, ()> {
    let Some(value) = headers.get("content-range") else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let value = value.strip_prefix("bytes ").unwrap_or(value);
    let (start, end) = value.split_once('-').ok_or(())?;
    let start: u64 = start.trim().parse().map_err(|_| ())?;
    let end: u64 = end.trim().parse().map_err(|_| ())?;
    if end < start {
        return Err(());
    }
    Ok(Some(start))
}

fn chunk_error(r: &OciRef, upload: &str, err: OciWriteError) -> AppResult<Response> {
    Ok(match err {
        OciWriteError::NotFound => unknown_upload(upload),
        OciWriteError::OutOfRange { received } => {
            let mut response = oci_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                &format!("the chunk must start at {received}"),
            );
            if let Ok(value) = format!("0-{}", received.saturating_sub(1)).parse() {
                response.headers_mut().insert("Range", value);
            }
            if let Ok(value) = location(r, upload).parse() {
                response.headers_mut().insert("Location", value);
            }
            response
        }
        OciWriteError::TooManySegments => oci_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "SIZE_INVALID",
            "too many chunks in one upload",
        ),
        OciWriteError::DigestMismatch { computed } => oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            &format!("digest mismatch: computed {computed}"),
        ),
        OciWriteError::Empty => oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_INVALID",
            "no blob data provided",
        ),
        other => return Err(other.into()),
    })
}

pub async fn start_upload(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;

    let upload_id = state.ids.upload_id();
    let prefix = layout::upload_prefix(&repo_prefix(&state, &repo).await?, &upload_id);
    state
        .oci
        .start_upload(&upload_id, repo.id, &r.name, &prefix, state.clock.now())
        .await?;

    let min_chunk = state.storage.upload_plan().min_chunk_bytes;
    let mut response = progress(&r, &upload_id, 0, StatusCode::ACCEPTED);
    if let Ok(value) = min_chunk.to_string().parse() {
        response.headers_mut().insert("OCI-Chunk-Min-Length", value);
    }
    Ok(response)
}

/// `GET` on an upload: where it stands, for a client resuming it.
pub async fn upload_status(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let upload = param(&params, "uuid")?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;
    match state.oci.upload(upload).await? {
        Some(session) if session.repository == repo.id => {
            Ok(progress(&r, upload, session.received, StatusCode::NO_CONTENT))
        }
        _ => Ok(unknown_upload(upload)),
    }
}

pub async fn upload_chunk(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    auth: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let upload = param(&params, "uuid")?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;
    let Ok(start) = chunk_start(&headers) else {
        return Ok(oci_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "BLOB_UPLOAD_INVALID",
            "invalid Content-Range",
        ));
    };

    let appended = AppendChunk::new(state.oci.clone(), state.storage.clone())
        .run(upload, repo.id, start, body, state.clock.now())
        .await;
    match appended {
        Ok(received) => Ok(progress(&r, upload, received, StatusCode::ACCEPTED)),
        Err(err) => chunk_error(&r, upload, err),
    }
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
    let upload = param(&params, "uuid")?;
    let (repo, _) = writable_repo(&state, &r, auth).await?;
    let Ok(digest) = parse_digest(&query.digest) else {
        return Ok(oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            &format!("invalid digest: '{}'", query.digest),
        ));
    };
    let prefix = repo_prefix(&state, &repo).await?;

    let completed = CompleteUpload::new(state.oci.clone(), state.storage.clone(), state.placer())
        .run(
            Completion {
                upload,
                repository: repo.id,
                repo_prefix: &prefix,
                digest: &digest,
                content_type: "application/octet-stream",
                body,
            },
            state.clock.now(),
        )
        .await;
    if let Err(err) = completed {
        return chunk_error(&r, upload, err);
    }
    info!(digest = %digest, image = %r.image_name(), "OCI blob uploaded");

    Ok((
        StatusCode::CREATED,
        [
            ("Docker-Content-Digest", digest.clone()),
            ("Content-Length", "0".to_string()),
            ("Location", format!("/v2/{}/blobs/{}", r.image_name(), digest)),
        ],
    )
        .into_response())
}
