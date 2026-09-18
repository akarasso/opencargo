use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

use crate::auth::middleware::AuthUser;
use crate::domain::{DomainError, Format, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::registry::cx;
use crate::registry::resolve::{collect, first_hit, probe_access, Collected, ResolveError};
use crate::server::AppState;

use super::leaves::{Coordinates, EntriesLeaf, NupkgLeaf, NuspecLeaf};
use super::model::{self, Entry};
use super::render::{self, Base};

pub(super) fn rules() -> &'static dyn crate::domain::FormatRules {
    &super::rules::NugetRules
}

/// The lowercase id of a request, refused before any lookup when it is not
/// a NuGet id.
pub(super) fn id_of(raw: &str) -> AppResult<String> {
    Ok(rules().admit(raw)?)
}

pub(super) fn key_of(raw: &str) -> AppResult<String> {
    rules().validate_version(raw)?;
    Ok(rules().normalize_version(raw))
}

/// A NuGet repository the caller may read; an anonymous caller on a group
/// with a member it cannot read is asked for credentials (NuGet 2.3).
pub(super) async fn open(
    state: &AppState,
    repo_name: &str,
    auth: Option<&AuthUser>,
) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Nuget)?;
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;
    match probe_access(&cx(state, auth, &repo), &repo).await {
        Ok(()) => Ok(repo),
        Err(ResolveError::Domain(DomainError::Forbidden(_))) => Err(AppError::Unauthorized(
            "authentication required to read this repository".to_string(),
        )),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn base(state: &AppState, repo: &Repository) -> Base {
    Base::new(
        &state.base_url,
        &repo.name,
        repo.kind().ok() == Some(RepoKind::Hosted),
    )
}

fn warned(mut response: Response, degraded: Option<String>) -> Response {
    let warning = degraded.and_then(|why| HeaderValue::from_str(&format!("199 - \"{why}\"")).ok());
    if let Some(value) = warning {
        response.headers_mut().insert(header::WARNING, value);
    }
    response
}

/// Every version every readable member knows, merged by key.
pub(super) async fn entries(
    state: &AppState,
    repo: &Repository,
    auth: Option<&AuthUser>,
    id: &str,
) -> AppResult<(Vec<Entry>, Option<String>)> {
    let leaf = EntriesLeaf { id: id.to_string() };
    let Collected { hits, degraded } = collect(&cx(state, auth, repo), repo, &leaf).await?;
    let merged = model::merge(hits);
    if merged.is_empty() {
        return Err(AppError::NotFound(format!("package not found: {id}")));
    }
    Ok((merged, degraded))
}

pub async fn service_index(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    Ok(Json(base(&state, &repo).service_index()))
}

pub async fn flat_index(
    State(state): State<AppState>,
    Path((repo_name, id)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let repo = open(&state, &repo_name, auth).await?;
    let (entries, degraded) = entries(&state, &repo, auth, &id).await?;
    Ok(warned(Json(render::flat_index(&entries)).into_response(), degraded))
}

/// `{id}.{version}.nupkg` or `{id}.nuspec` under `/{id}/{version}/`.
pub async fn flat_file(
    State(state): State<AppState>,
    Path((repo_name, id, version, file)): Path<(String, String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let key = key_of(&version)?;
    let file = file.to_ascii_lowercase();
    let at = Coordinates {
        id: id.clone(),
        key: Some(key.clone()),
    };
    let repo = open(&state, &repo_name, auth).await?;
    let cx = cx(&state, auth, &repo);
    let (mut payload, content_type) = if file == format!("{id}.{key}.nupkg") {
        (first_hit(&cx, &repo, &NupkgLeaf { at }).await?, "application/octet-stream")
    } else if file == format!("{id}.nuspec") {
        (first_hit(&cx, &repo, &NuspecLeaf { at }).await?, "application/xml")
    } else {
        return Err(AppError::NotFound(format!("no such file: {file}")));
    };
    payload.content_type = Some(content_type.to_string());
    state.proxy.stream_response(&payload, Vec::new()).await
}

pub async fn registration_index(
    State(state): State<AppState>,
    Path((repo_name, id)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let repo = open(&state, &repo_name, auth).await?;
    let (entries, degraded) = entries(&state, &repo, auth, &id).await?;
    let doc = base(&state, &repo).registration_index_doc(&id, &entries);
    Ok(warned(Json(doc).into_response(), degraded))
}

pub async fn registration_leaf(
    State(state): State<AppState>,
    Path((repo_name, id, leaf)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let version = leaf
        .strip_suffix(".json")
        .ok_or_else(|| AppError::NotFound(format!("no such document: {leaf}")))?;
    let key = key_of(version)?;
    let repo = open(&state, &repo_name, auth).await?;
    let (entries, degraded) = entries(&state, &repo, auth, &id).await?;
    let entry = entries
        .iter()
        .find(|e| e.key == key)
        .ok_or_else(|| AppError::NotFound(format!("version not found: {id} {key}")))?;
    let doc = base(&state, &repo).registration_leaf_doc(entry);
    Ok(warned(Json(doc).into_response(), degraded))
}

pub async fn registration_page(
    State(state): State<AppState>,
    Path((repo_name, id, lower, upper)): Path<(String, String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let upper = upper
        .strip_suffix(".json")
        .ok_or_else(|| AppError::NotFound(format!("no such page: {upper}")))?;
    let repo = open(&state, &repo_name, auth).await?;
    let (entries, degraded) = entries(&state, &repo, auth, &id).await?;
    let doc = base(&state, &repo)
        .registration_page_doc(&id, &entries, &lower, upper)
        .ok_or_else(|| AppError::NotFound(format!("no such page: {lower}/{upper}")))?;
    Ok(warned((StatusCode::OK, Json(doc)).into_response(), degraded))
}
