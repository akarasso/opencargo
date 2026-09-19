use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tracing::info;

use crate::app::oci::{DeleteManifest, ManifestTarget, OciWriteError, PushedManifest, PutManifest};
use crate::app::publish_tail::{PreScan, Published};
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::oci::OciStore;
use crate::registry::cx;
use crate::registry::resolve::first_hit;
use crate::server::AppState;

use super::leaves::ManifestLeaf;
use super::uploads::repo_prefix;
use super::{is_digest, oci_error, param, parse_digest, refs, respond, sha256_digest, OciRef};

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
        crate::registry::rules::rules_of(crate::domain::Format::Oci)?.validate_version(reference)?;
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
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;

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
    crate::registry::ensure_can_write(&state.authorize(), &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Oci)?;
    // The one request of a push that makes the image exist: the count is
    // an image count, and the blobs before it are not counted.
    crate::registry::meter_publish(&state, &auth_user, Format::Oci, &r.repo)?;

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

    if let Some(refused) =
        push_manifest(&state, &repo, &r, &reference, &digest, &content_type, &body).await?
    {
        return Ok(refused);
    }
    // The "package" is the image name, the "version" the pushed reference;
    // OCI has no `versions` row, so no vulnerability scan applies.
    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Oci,
                repository: &r.repo,
                package: &r.name,
                version: &reference,
                version_id: None,
                metadata_json: &String::from_utf8_lossy(&body),
                published_by: &auth_user.username,
            },
            PreScan::default(),
            chrono::Utc::now(),
        )
        .await;
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

/// The bytes, then the rows that claim them; `Some` is the refusal a client
/// reads as its blobs being unknown.
async fn push_manifest(
    state: &AppState,
    repo: &Repository,
    r: &OciRef,
    reference: &str,
    digest: &str,
    content_type: &str,
    body: &Bytes,
) -> AppResult<Option<Response>> {
    let prefix = repo_prefix(state, repo).await?;
    let (blobs, children) = refs::split_refs(body);
    let pushed = PutManifest::new(state.oci.clone(), state.placer())
        .run(
            PushedManifest {
                repository: repo.id,
                repo_prefix: &prefix,
                name: &r.name,
                digest,
                content_type,
                blobs,
                children,
                tag: (!is_digest(reference)).then_some(reference),
                body: body.clone(),
            },
            state.clock.now(),
        )
        .await;
    match pushed {
        Ok(()) => Ok(None),
        Err(OciWriteError::BlobUnknown) => Ok(Some(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_BLOB_UNKNOWN",
            "the manifest references a blob this repository does not hold",
        ))),
        Err(err) => Err(err.into()),
    }
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
    crate::registry::ensure_action(
        &state.authorize(),
        &repo,
        &auth_user,
        crate::domain::RepoAction::Delete,
    )
    .await?;
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
    DeleteManifest::new(state.oci.clone())
        .run(
            ManifestTarget {
                repository: repo.id,
                name: &r.name,
                digest: &digest,
            },
            state.clock.now(),
        )
        .await
        .map_err(|err| match err {
            OciWriteError::NotFound => AppError::NotFound(format!(
                "manifest not found: {}@{} in {}",
                r.name, digest, r.repo
            )),
            other => other.into(),
        })?;

    Ok(StatusCode::ACCEPTED.into_response())
}
