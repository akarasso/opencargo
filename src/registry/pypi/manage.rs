//! Yank, unyank and delete: PyPI has no client for them, so they are this
//! server's own routes under `/{repo}/pypi/`, gated like an upload.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::app::pypi::{DeleteRelease, YankRelease};
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::AppError;
use crate::registry::rules::rules_of;
use crate::server::AppState;

use super::PypiResult;

async fn gate(
    state: &AppState,
    repo_name: &str,
    project: &str,
    auth: Option<axum::Extension<AuthUser>>,
    action: crate::domain::RepoAction,
) -> PypiResult<Repository> {
    let auth = auth
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Pypi)?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_action(&state.authorize(), &repo, Some(project), &auth, action).await?;
    Ok(repo)
}

fn canonical(project: &str, version: Option<&str>) -> PypiResult<(String, Option<String>)> {
    let rules = rules_of(Format::Pypi)?;
    let project = rules.admit(project)?;
    let version = match version {
        Some(v) => {
            rules.validate_version(v)?;
            Some(rules.normalize_version(v))
        }
        None => None,
    };
    Ok((project, version))
}

async fn set_yanked(
    state: &AppState,
    repo: &Repository,
    project: &str,
    version: &str,
    reason: Option<&str>,
    yanked: bool,
) -> PypiResult<Response> {
    let (project, version) = canonical(project, Some(version))?;
    let version = version.unwrap_or_default();
    YankRelease::new(state.pypi.clone())
        .run(repo.id, &project, &version, reason, yanked, chrono::Utc::now())
        .await?;
    Ok(Json(json!({"ok": true, "project": project, "version": version, "yanked": yanked})).into_response())
}

pub async fn yank(
    State(state): State<AppState>,
    Path((repo_name, project, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> PypiResult<Response> {
    let repo = gate(&state, &repo_name, &project, auth, crate::domain::RepoAction::Write).await?;
    let reason = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_string));
    set_yanked(&state, &repo, &project, &version, reason.as_deref(), true).await
}

pub async fn unyank(
    State(state): State<AppState>,
    Path((repo_name, project, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> PypiResult<Response> {
    let repo = gate(&state, &repo_name, &project, auth, crate::domain::RepoAction::Write).await?;
    set_yanked(&state, &repo, &project, &version, None, false).await
}

pub async fn delete_release(
    State(state): State<AppState>,
    Path((repo_name, project, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> PypiResult<Response> {
    let repo = gate(&state, &repo_name, &project, auth, crate::domain::RepoAction::Delete).await?;
    let (project, version) = canonical(&project, Some(&version))?;
    let released = DeleteRelease::new(state.pypi.clone())
        .run(repo.id, &project, version.as_deref(), chrono::Utc::now())
        .await?;
    Ok((StatusCode::OK, Json(json!({"ok": true, "released": released.len()}))).into_response())
}

pub async fn delete_project(
    State(state): State<AppState>,
    Path((repo_name, project)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> PypiResult<Response> {
    let repo = gate(&state, &repo_name, &project, auth, crate::domain::RepoAction::Delete).await?;
    let (project, _) = canonical(&project, None)?;
    let released = DeleteRelease::new(state.pypi.clone())
        .run(repo.id, &project, None, chrono::Utc::now())
        .await?;
    Ok((StatusCode::OK, Json(json!({"ok": true, "released": released.len()}))).into_response())
}
