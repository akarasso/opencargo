//! `/raw/{repo}/{*path}`: GET and HEAD read, PUT deposits, DELETE removes.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;

use super::leaves::{FileLeaf, DEFAULT_CONTENT_TYPE};
use crate::app::publish_tail::{PreScan, Published};
use crate::app::raw::{Deposit, Deposited};
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::first_hit;
use crate::server::AppState;

/// The digest a client may declare for the bytes it sends, and the one the
/// server answers a read with.
pub const CHECKSUM_HEADER: &str = "x-checksum-sha256";

async fn open(state: &AppState, name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Raw)?;
    Ok(repo)
}

/// Kept only when it is a header value a client could have sent as is.
fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 255 && v.is_ascii())
        .map(str::to_string)
}

fn declared_sha256(headers: &HeaderMap) -> AppResult<Option<String>> {
    let Some(value) = headers.get(CHECKSUM_HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            AppError::BadRequest(format!("{CHECKSUM_HEADER} is not a sha256 hex digest"))
        })?;
    Ok(Some(value))
}

/// A raw body is the only one whose type its uploader chooses, so a read is
/// a download: nothing a client stored is ever rendered on this origin.
fn disposition(path: &str) -> HeaderValue {
    let name = crate::domain::layout::file_name(path);
    HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

fn digest_headers(digest: Option<&str>) -> Vec<(HeaderName, HeaderValue)> {
    let Some(digest) = digest else {
        return Vec::new();
    };
    let mut extra = Vec::new();
    if let Ok(value) = HeaderValue::from_str(digest) {
        extra.push((HeaderName::from_static(CHECKSUM_HEADER), value));
    }
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{digest}\"")) {
        extra.push((header::ETAG, etag));
    }
    extra
}

/// GET and HEAD, on any kind of repository: a hosted one serves its own
/// bytes, a proxy relays its upstream through the cache, a group walks its
/// members.
pub async fn read(
    State(state): State<AppState>,
    Path((repo_name, path)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let path = super::admit(&path)?.to_string();
    let repo = open(&state, &repo_name).await?;
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;
    let cx = crate::registry::cx(&state, auth, &repo);
    let attachment = disposition(&path);
    let mut payload = first_hit(&cx, &repo, &FileLeaf { path }).await?;
    if payload.content_type.is_none() {
        payload.content_type = Some(DEFAULT_CONTENT_TYPE.to_string());
    }
    let mut extra = digest_headers(payload.digest.as_deref());
    extra.push((header::CONTENT_DISPOSITION, attachment));
    state.proxy.stream_response(&payload, extra).await
}

/// PUT: the bytes of one path, into a hosted repository.
pub async fn put(
    State(state): State<AppState>,
    Path((repo_name, path)): Path<(String, String)>,
    request: axum::http::Request<Body>,
) -> AppResult<Response> {
    let auth = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let path = super::admit(&path)?.to_string();
    let repo = open(&state, &repo_name).await?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, &auth).await?;
    crate::registry::ensure_hosted(&repo)?;
    let content_type = content_type(request.headers());
    let declared = declared_sha256(request.headers())?;
    let stream = request
        .into_body()
        .into_data_stream()
        .map(|chunk| chunk.map_err(|e| e.to_string()));
    let deposited = state
        .put_raw_file()
        .run(
            Deposit {
                repository: repo.id,
                path: &path,
                content_type: content_type.as_deref(),
                declared_sha256: declared.as_deref(),
                principal: &auth.username,
            },
            Box::pin(stream),
            state.clock.now(),
        )
        .await?;
    let status = match &deposited {
        Deposited::Stored { .. } => StatusCode::CREATED,
        Deposited::Unchanged(_) => StatusCode::OK,
    };
    if matches!(deposited, Deposited::Stored { .. }) {
        announce(&state, &repo, &path, &auth.username).await;
    }
    let extra = digest_headers(Some(&deposited.file().sha256));
    let mut response = status.into_response();
    for (name, value) in extra {
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}

/// DELETE: one path of a hosted repository. The bytes it held are enqueued
/// for reclamation, never deleted here.
pub async fn delete(
    State(state): State<AppState>,
    Path((repo_name, path)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let path = super::admit(&path)?.to_string();
    let repo = open(&state, &repo_name).await?;
    crate::registry::ensure_can_delete(&state.authorize(), &repo, &auth).await?;
    crate::registry::ensure_hosted(&repo)?;
    state
        .delete_raw_file()
        .run(repo.id, &path, state.clock.now())
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The shared publish tail: a raw file has no version, so it announces the
/// path alone.
async fn announce(state: &AppState, repo: &Repository, path: &str, by: &str) {
    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Raw,
                repository: &repo.name,
                package: path,
                version: "",
                version_id: None,
                metadata_json: "{}",
                published_by: by,
            },
            PreScan::default(),
            state.clock.now(),
        )
        .await;
}
