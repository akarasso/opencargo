//! The legacy upload API (`POST /{repo}/legacy/`) twine and poetry speak.
//! The form only carries the file and its claimed digest; name, version and
//! requirements come from the archive.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::app::publish_tail::Published;
use crate::app::pypi::{Upload, Uploaded};
use crate::auth::middleware::AuthUser;
use crate::domain::Format;
use crate::error::AppError;
use crate::server::AppState;

use super::metadata::inspect;
use super::multipart;
use super::names::parse_filename;
use super::{PypiError, PypiResult};

const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

fn bad(why: impl Into<String>) -> PypiError {
    AppError::BadRequest(why.into()).into()
}

struct Form {
    filename: String,
    content: Bytes,
    sha256: Option<String>,
}

fn form(content_type: &str, body: &Bytes) -> PypiResult<Form> {
    let boundary = multipart::boundary(content_type)
        .ok_or_else(|| bad("expected a multipart/form-data upload"))?;
    let parts = multipart::parse(body, &boundary).ok_or_else(|| bad("malformed multipart body"))?;
    let field = |name: &str| {
        parts
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| std::str::from_utf8(&p.body).ok())
            .map(|v| v.trim().to_string())
    };
    if field(":action").as_deref() != Some("file_upload") {
        return Err(bad("unsupported :action, expected file_upload"));
    }
    let content = parts
        .iter()
        .find(|p| p.name == "content")
        .ok_or_else(|| bad("the upload carries no content"))?;
    let filename = content
        .filename
        .clone()
        .ok_or_else(|| bad("the content part names no file"))?;
    Ok(Form {
        filename,
        content: content.body.clone(),
        sha256: field("sha256_digest").filter(|d| !d.is_empty()),
    })
}

pub async fn upload(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> PypiResult<Response> {
    let auth = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    if !state.publish_rate_limiter.check(&format!("publish:{}", auth.username)) {
        return Err(AppError::TooManyRequests("too many publish requests, try again later".to_string()).into());
    }
    let repo = crate::registry::load_repo(state.repos.as_ref(), &repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Pypi)?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth).await?;

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(request.into_body(), MAX_UPLOAD_BYTES)
        .await
        .map_err(|e| bad(format!("failed to read body: {e}")))?;
    let form = form(&content_type, &body)?;
    let file = parse_filename(&form.filename)?;
    if let Some(claimed) = &form.sha256 {
        let actual = format!("{:x}", Sha256::digest(&form.content));
        if !claimed.eq_ignore_ascii_case(&actual) {
            return Err(bad("sha256_digest does not match the uploaded file"));
        }
    }

    let inspected = {
        let permit = state
            .archive_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::ServiceUnavailable("archive inspection unavailable".to_string()))?;
        let (file, content) = (file.clone(), form.content.clone());
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            inspect(&file, &content)
        })
        .await
        .map_err(|e| AppError::Internal(format!("archive inspection failed: {e}")))??
    };
    let metadata_json = inspected.metadata.to_json().to_string();
    let pre_scan = state.publish_gate().run(Format::Pypi, &metadata_json).await?;

    let version = file.version.canonical();
    let uploaded = state
        .publish_pypi_file()
        .run(
            Upload {
                repository: repo.id,
                project: &file.project,
                summary: inspected.metadata.summary.as_deref(),
                version: &version,
                metadata_json: &metadata_json,
                filename: &file.canonical,
                packagetype: file.kind.packagetype(),
                requires_python: inspected.metadata.requires_python.as_deref(),
                bytes: form.content,
                metadata: inspected.raw.map(Bytes::from),
            },
            chrono::Utc::now(),
        )
        .await
        .map_err(|err| match err {
            crate::app::publish::PublishError::Store(crate::error::StoreError::Conflict) => {
                PypiError::App(AppError::Conflict(format!(
                    "File already exists: '{}' holds other bytes",
                    file.canonical
                )))
            }
            other => other.into(),
        })?;

    let Uploaded::Created(published) = uploaded else {
        return Ok((StatusCode::OK, "OK").into_response());
    };
    info!(project = %file.project, file = %file.canonical, repo = %repo.name, "PyPI file published");
    if published.version_created {
        state
            .publish_tail()
            .run(
                &Published {
                    format: Format::Pypi,
                    repository: &repo.name,
                    package: &file.project,
                    version: &version,
                    version_id: Some(published.file.version_id),
                    metadata_json: &metadata_json,
                    published_by: &auth.username,
                },
                pre_scan,
                chrono::Utc::now(),
            )
            .await;
    }
    Ok((StatusCode::OK, "OK").into_response())
}
