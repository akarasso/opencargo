use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

use crate::auth::middleware::AuthUser;
use crate::domain::Repository;
use crate::error::{AppError, AppResult};
use crate::registry::cx;
use crate::registry::resolve::{collect, first_hit, Collected};
use crate::server::AppState;

use super::compare_versions;
use super::escape::{validate_escaped_module, validate_escaped_version};
use super::leaves::{FileLeaf, LatestLeaf, ListLeaf};
use super::upstream::FileKind;


/// Every read validates the escaped module before any key or URL is built,
/// then enforces read access once.
async fn open(
    state: &AppState,
    repo_name: &str,
    module: &str,
    auth: Option<&AuthUser>,
) -> AppResult<Repository> {
    validate_escaped_module(module)?;
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;
    Ok(repo)
}

fn not_found(module: &str, repo: &Repository) -> AppError {
    AppError::NotFound(format!(
        "module not found: {module} in repository '{}'",
        repo.name
    ))
}

fn with_warning(mut response: Response, degraded: Option<String>) -> Response {
    let warning = degraded.and_then(|why| HeaderValue::from_str(&format!("199 - \"{why}\"")).ok());
    if let Some(value) = warning {
        response.headers_mut().insert(header::WARNING, value);
    }
    response
}

pub async fn list_versions(
    State(state): State<AppState>,
    Path((repo_name, module)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, &module, auth).await?;
    let leaf = ListLeaf {
        module: module.clone(),
    };
    let Collected { hits, degraded } = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;
    if hits.is_empty() {
        return Err(not_found(&module, &repo));
    }
    let mut seen = HashSet::new();
    let lines: Vec<String> = hits
        .into_iter()
        .flatten()
        .filter(|v| seen.insert(v.clone()))
        .collect();
    let response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        lines.join("\n"),
    )
        .into_response();
    Ok(with_warning(response, degraded))
}

pub async fn latest_version(
    State(state): State<AppState>,
    Path((repo_name, module)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, &module, auth).await?;
    let leaf = LatestLeaf {
        module: module.clone(),
    };
    let Collected { hits, degraded } = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;
    let version_of = |info: &Value| info["Version"].as_str().unwrap_or_default().to_string();
    let latest = hits
        .into_iter()
        .max_by(|a, b| compare_versions(&version_of(a), &version_of(b)))
        .ok_or_else(|| not_found(&module, &repo))?;
    Ok(with_warning(Json(latest).into_response(), degraded))
}

/// `.info`, `.mod` and `.zip` share one `/{repo}/{module}/@v/{version}` route.
pub async fn version_dispatch(
    State(state): State<AppState>,
    Path((repo_name, module, version_raw)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let (version, kind) = FileKind::split(&version_raw).ok_or_else(|| {
        AppError::BadRequest(
            "unknown version file extension; expected .info, .mod, or .zip".to_string(),
        )
    })?;
    validate_escaped_version(version)?;
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, &module, auth).await?;
    let leaf = FileLeaf {
        module,
        version: version.to_string(),
        kind,
    };
    let mut payload = first_hit(&cx(&state, auth, &repo), &repo, &leaf).await?;
    payload.content_type = Some(kind.content_type().to_string());
    let extra = match kind {
        FileKind::Zip => vec![(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{version}.zip\""))
                .map_err(|_| AppError::BadRequest(format!("invalid version: '{version}'")))?,
        )],
        FileKind::Info | FileKind::Mod => Vec::new(),
    };
    state.proxy.stream_response(&payload, extra).await
}
