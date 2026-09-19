use axum::{
    extract::{Path, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use serde_json::Value;

use crate::auth::middleware::AuthUser;
use crate::domain::{CacheRepo, DomainError, Format, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::registry::cx;
use crate::registry::resolve::{collect, first_hit, probe_access, view, Collected, ResolveError, Subject};
use crate::server::AppState;

use super::leaves::{hosted_stamp, Coordinates, EntriesLeaf, NupkgLeaf, NuspecLeaf};
use super::merged::{DocKey, Sources};
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
    id: Option<&str>,
    auth: Option<&AuthUser>,
) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Nuget)?;
    crate::registry::ensure_can_read(&state.authorize(), &repo, id, auth).await?;
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

/// A document rendered from the merged entries, through the registration
/// memo: keyed by the repository, the id, the caller's view of the group
/// and the document; valid while the hosted members' stamps hold and, when
/// a proxy member is in the view, within the age cap. An answer missing a
/// member that is down is served, never kept.
async fn rendered<F>(
    state: &AppState,
    repo: &Repository,
    auth: Option<&AuthUser>,
    id: &str,
    doc: String,
    render: F,
) -> AppResult<Response>
where
    F: FnOnce(&[Entry]) -> AppResult<Value>,
{
    let cx = cx(state, auth, repo);
    let members = view(&cx, repo, &Subject::of(id)).await?;
    let mut sources = Sources::default();
    for member in &members {
        match member.kind()? {
            RepoKind::Hosted => sources.stamps.push(hosted_stamp(&cx, CacheRepo(member), id).await?),
            _ => sources.upstream = true,
        }
    }
    let key = DocKey {
        repo: repo.id,
        repo_name: repo.name.clone(),
        id: id.to_string(),
        view: members.iter().map(|m| m.id).collect(),
        doc,
        routes: state.routing.version(),
    };
    let mut degraded = None;
    let body = state
        .nuget_documents
        .get_or_compute(key, sources, || async {
            let (entries, why) = entries(state, repo, auth, id).await?;
            let body = Bytes::from(serde_json::to_vec(&render(&entries)?)?);
            let keep = why.is_none();
            degraded = why;
            Ok::<_, AppError>((body, keep))
        })
        .await?;
    let response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    Ok(warned(response, degraded))
}

pub async fn service_index(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, None, auth).await?;
    Ok(Json(base(&state, &repo).service_index()))
}

pub async fn flat_index(
    State(state): State<AppState>,
    Path((repo_name, id)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let id = id_of(&id)?;
    let repo = open(&state, &repo_name, Some(&id), auth).await?;
    rendered(&state, &repo, auth, &id, "flat".to_string(), |entries| {
        Ok(render::flat_index(entries))
    })
    .await
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
    let repo = open(&state, &repo_name, Some(&id), auth).await?;
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
    let repo = open(&state, &repo_name, Some(&id), auth).await?;
    let base = base(&state, &repo);
    rendered(&state, &repo, auth, &id, "index".to_string(), |entries| {
        Ok(base.registration_index_doc(&id, entries))
    })
    .await
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
    let repo = open(&state, &repo_name, Some(&id), auth).await?;
    let base = base(&state, &repo);
    rendered(&state, &repo, auth, &id, format!("leaf/{key}"), |entries| {
        let entry = entries
            .iter()
            .find(|e| e.key == key)
            .ok_or_else(|| AppError::NotFound(format!("version not found: {id} {key}")))?;
        Ok(base.registration_leaf_doc(entry))
    })
    .await
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
    let repo = open(&state, &repo_name, Some(&id), auth).await?;
    let base = base(&state, &repo);
    rendered(&state, &repo, auth, &id, format!("page/{lower}/{upper}"), |entries| {
        base.registration_page_doc(&id, entries, &lower, upper)
            .ok_or_else(|| AppError::NotFound(format!("no such page: {lower}/{upper}")))
    })
    .await
}
