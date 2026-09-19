use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue},
    response::Response,
};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::first_hit;
use crate::registry::{cx, extract_package_name};
use crate::server::AppState;

use super::leaves::{PackumentLeaf, TarballLeaf};
use super::param;
use super::render::ABBREVIATED_TYPE;

/// `{name}-{version}.tgz`: one path segment of version characters, so it is
/// safe in a cache key and an upstream URL.
fn validate_tarball_filename(filename: &str) -> AppResult<()> {
    let stem = filename.strip_suffix(".tgz").unwrap_or_default();
    if stem.is_empty()
        || filename.len() > 255
        || !stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(AppError::BadRequest(format!(
            "invalid tarball filename: '{filename}'"
        )));
    }
    Ok(())
}

pub async fn get_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let repo_name = param(&params, "repo")?;
    let package_name = extract_package_name(&params);
    crate::domain::validate_npm_read_name(&package_name)?;

    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;

    let abbreviated = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(ABBREVIATED_TYPE));
    let leaf = PackumentLeaf {
        name: package_name,
        abbreviated,
    };
    let packument = first_hit(&cx(&state, auth, &repo), &repo, &leaf).await?;
    state.proxy.stream_response(&packument, Vec::new()).await
}

pub async fn download_tarball(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let repo_name = param(&params, "repo")?;
    let package_name = extract_package_name(&params);
    let filename = param(&params, "filename")?;
    crate::domain::validate_npm_read_name(&package_name)?;
    validate_tarball_filename(filename)?;

    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;

    let leaf = TarballLeaf {
        name: package_name,
        filename: filename.to_string(),
    };
    let mut payload = first_hit(&cx(&state, auth, &repo), &repo, &leaf).await?;
    payload.content_type = Some("application/octet-stream".to_string());
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|_| AppError::BadRequest(format!("invalid tarball filename: '{filename}'")))?;
    state
        .proxy
        .stream_response(&payload, vec![(header::CONTENT_DISPOSITION, disposition)])
        .await
}
