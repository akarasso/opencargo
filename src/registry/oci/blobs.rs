use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::app::oci::{DeleteBlob, OciWriteError};
use crate::auth::middleware::AuthUser;
use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::registry::cx;
use crate::registry::resolve::first_hit;
use crate::server::AppState;

use super::leaves::BlobLeaf;
use super::{param, parse_digest, respond, OciRef};

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
    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_read(&state.authorize(), &repo, Some(&r.name), auth).await?;

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

    let repo = crate::registry::load_repo(state.repos.as_ref(), &r.repo).await?;
    crate::registry::ensure_can_delete(&state.authorize(), &repo, None, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;

    DeleteBlob::new(state.oci.clone())
        .run(repo.id, &digest, state.clock.now())
        .await
        .map_err(|err| match err {
            OciWriteError::NotFound => AppError::NotFound(format!(
                "blob not found: {} in {}",
                digest,
                r.image_name()
            )),
            OciWriteError::Referenced => AppError::Conflict(format!(
                "blob {digest} is still referenced by a manifest; delete those manifests first"
            )),
            other => other.into(),
        })?;

    Ok(StatusCode::ACCEPTED.into_response())
}
