use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use sha2::{Digest, Sha256};

use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::AppError;
use crate::registry::cx;
use crate::registry::resolve::{collect, first_hit, Collected};
use crate::server::AppState;

use super::leaves::{merge, FileLeaf, IndexLeaf, PageLeaf, Served};
use super::names::{is_valid_name, normalize, parse_filename};
use super::simple::{negotiate, render_index, render_page, Flavor};
use super::{PypiError, PypiResult};

async fn open(state: &AppState, repo_name: &str, auth: Option<&AuthUser>) -> Result<Repository, AppError> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;
    crate::registry::ensure_format(&repo, Format::Pypi)?;
    Ok(repo)
}

fn flavor(headers: &HeaderMap) -> Result<Flavor, PypiError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    negotiate(accept).ok_or(PypiError::NotAcceptable)
}

/// A strong validator over the rendered body, answered with 304 when the
/// client holds it.
fn page_response(body: String, flavor: Flavor, headers: &HeaderMap, degraded: Option<String>) -> Response {
    let etag = format!("\"{:x}\"", Sha256::digest(body.as_bytes()));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let mut response = if matches {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, flavor.content_type())], body).into_response()
    };
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::VARY, HeaderValue::from_static("Accept"));
    if let Some(v) = degraded.and_then(|why| HeaderValue::from_str(&format!("199 - \"{why}\"")).ok()) {
        h.insert(header::WARNING, v);
    }
    response
}

pub async fn index(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<axum::Extension<AuthUser>>,
    headers: HeaderMap,
) -> PypiResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let flavor = flavor(&headers)?;
    let repo = open(&state, &repo_name, auth).await?;
    let leaf = IndexLeaf {
        files: state.pypi.as_ref(),
    };
    let Collected { hits, degraded } = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;
    let mut projects: Vec<String> = hits.into_iter().flatten().collect();
    projects.sort();
    projects.dedup();
    Ok(page_response(render_index(&projects, flavor), flavor, &headers, degraded))
}

pub async fn project_no_slash(Path((repo, project)): Path<(String, String)>) -> Redirect {
    Redirect::permanent(&format!("/{repo}/simple/{project}/"))
}

pub async fn project(
    State(state): State<AppState>,
    Path((repo_name, raw)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
    headers: HeaderMap,
) -> PypiResult<Response> {
    if !is_valid_name(&raw) {
        return Err(AppError::NotFound(format!("no project named '{raw}'")).into());
    }
    let project = normalize(&raw);
    if project != raw {
        return Ok(Redirect::permanent(&format!("/{repo_name}/simple/{project}/")).into_response());
    }
    let auth = auth.as_ref().map(|e| &e.0);
    let flavor = flavor(&headers)?;
    let repo = open(&state, &repo_name, auth).await?;
    let leaf = PageLeaf {
        files: state.pypi.as_ref(),
        project: project.clone(),
    };
    let Collected { hits, degraded } = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;
    if hits.is_empty() {
        return Err(AppError::NotFound(format!(
            "project not found: {project} in repository '{}'",
            repo.name
        ))
        .into());
    }
    let page = merge(&project, hits);
    Ok(page_response(render_page(&page, flavor), flavor, &headers, degraded))
}

pub async fn file(
    State(state): State<AppState>,
    Path((repo_name, project, requested)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> PypiResult<Response> {
    let (filename, metadata) = match requested.strip_suffix(".metadata") {
        Some(artifact) => (artifact.to_string(), true),
        None => (requested.clone(), false),
    };
    let parsed = parse_filename(&filename)?;
    if parsed.project != project {
        return Err(AppError::NotFound(format!("no file named '{requested}'")).into());
    }
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let leaf = FileLeaf {
        files: state.pypi.as_ref(),
        project,
        filename: filename.clone(),
        metadata,
    };
    let payload = match first_hit(&cx(&state, auth, &repo), &repo, &leaf).await? {
        Served::Payload(payload) => payload,
        Served::Absent => {
            return Err(AppError::NotFound(format!("no file named '{requested}'")).into());
        }
    };
    let mut payload = payload;
    payload.content_type = Some(
        if metadata {
            "text/plain; charset=utf-8"
        } else {
            "application/octet-stream"
        }
        .to_string(),
    );
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{requested}\""))
        .map_err(|_| AppError::BadRequest(format!("invalid filename: '{requested}'")))?;
    Ok(state
        .proxy
        .stream_response(&payload, vec![(header::CONTENT_DISPOSITION, disposition)])
        .await?)
}
