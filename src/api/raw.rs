//! `GET /api/v1/raw/{repo}/files`: what a raw repository holds, for the
//! admin UI and for any client that wants a listing rather than a path.

use axum::{
    extract::{Extension, Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::AuthUser;
use crate::domain::Format;
use crate::error::AppResult;
use crate::ports::raw::RawFile;
use crate::registry::resolve::{view, Subject};
use crate::server::AppState;
use crate::wire::wire_ts;

/// The window a listing reads per member before it reports itself
/// truncated; a raw repository is a tree, not a package index.
const SCAN: i64 = 1000;
const PAGE_SIZE: i64 = 50;

#[derive(Deserialize)]
pub struct FilesQuery {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    page: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileResponse {
    path: String,
    repository: String,
    size: i64,
    sha256: String,
    content_type: Option<String>,
    uploaded_by: String,
    uploaded_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesResponse {
    files: Vec<FileResponse>,
    total: i64,
    page: i64,
    page_size: i64,
    has_next: bool,
    /// A member held more than `SCAN` paths under the prefix.
    truncated: bool,
}

/// A hosted repository lists its own rows; a group merges its hosted
/// members in walk order, the first of them to hold a path winning it.
pub async fn list_files(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    Query(params): Query<FilesQuery>,
    auth: Option<Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = crate::registry::load_repo(state.repos.as_ref(), &repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Raw)?;
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;
    let cx = crate::registry::cx(&state, auth, &repo);

    let mut merged: Vec<(String, RawFile)> = Vec::new();
    let mut truncated = false;
    for member in view(&cx, &repo, &Subject::listing()).await? {
        let found = state.raw.list(member.id, &params.prefix, SCAN).await?;
        truncated |= found.len() as i64 == SCAN;
        for file in found {
            if !merged.iter().any(|(_, held)| held.path == file.path) {
                merged.push((member.name.clone(), file));
            }
        }
    }
    merged.sort_by(|a, b| a.1.path.cmp(&b.1.path));

    let page = params.page.unwrap_or(1).max(1);
    let offset = page.saturating_sub(1).saturating_mul(PAGE_SIZE);
    let total = merged.len() as i64;
    let files = merged
        .into_iter()
        .skip(offset.max(0) as usize)
        .take(PAGE_SIZE as usize)
        .map(|(repository, file)| FileResponse {
            path: file.path,
            repository,
            size: file.size,
            sha256: file.sha256,
            content_type: file.content_type,
            uploaded_by: file.uploaded_by,
            uploaded_at: wire_ts(file.uploaded_at),
        })
        .collect();

    Ok(Json(FilesResponse {
        files,
        total,
        page,
        page_size: PAGE_SIZE,
        has_next: offset.saturating_add(PAGE_SIZE) < total,
        truncated,
    }))
}
