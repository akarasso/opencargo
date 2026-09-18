use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::auth::middleware::AuthUser;
use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::first_hit;
use crate::server::AppState;

use super::leaves::BlobLeaf;
use super::{cx, param, parse_digest, paths, respond, OciRef};

pub async fn head_blob(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    serve_blob(&state, &params, auth.as_ref().map(|e| &e.0), true).await
}

pub async fn get_blob(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    serve_blob(&state, &params, auth.as_ref().map(|e| &e.0), false).await
}

async fn serve_blob(
    state: &AppState,
    params: &HashMap<String, String>,
    auth: Option<&AuthUser>,
    head: bool,
) -> AppResult<Response> {
    let r = OciRef::parse(params)?;
    let digest = parse_digest(param(params, "digest")?)?;
    let repo = crate::registry::load_repo(&state.db, &r.repo).await?;
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    let leaf = BlobLeaf {
        name: r.name,
        digest,
        head,
    };
    let payload = first_hit(&cx(state, auth, &repo), &repo, &leaf).await?;
    respond(state, payload).await
}

pub async fn delete_blob(
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
    let digest = parse_digest(param(&params, "digest")?)?;

    let repo = crate::registry::load_repo(&state.db, &r.repo).await?;
    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;

    // A blob still referenced by a manifest would break a live image.
    let refs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2",
    )
    .bind(repo.id)
    .bind(&digest)
    .fetch_one(&state.db)
    .await?;
    if refs > 0 {
        return Err(AppError::Conflict(format!(
            "blob {digest} is still referenced by {refs} manifest(s); delete those manifests first"
        )));
    }

    let result = sqlx::query("DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2")
        .bind(repo.id)
        .bind(&digest)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "blob not found: {} in {}",
            digest,
            r.image_name()
        )));
    }
    let _ = state
        .storage
        .delete(&paths::blob_path(&repo.name, &digest))
        .await;

    Ok(StatusCode::ACCEPTED.into_response())
}
