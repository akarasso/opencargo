use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::db::kinds::Format;
use crate::db::Package;
use crate::error::{AppError, AppResult};
use crate::registry::extract_package_name;
use crate::registry::resolve::first_hit;
use crate::server::AppState;

use super::leaves::DistTagsLeaf;
use super::{cx, param};

pub async fn get_dist_tags(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = param(&params, "repo")?;
    let package_name = extract_package_name(&params);

    crate::registry::validate_npm_read_name(&package_name)?;

    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    let leaf = DistTagsLeaf { name: package_name };
    let tags = first_hit(&cx(&state, auth, &repo), &repo, &leaf).await?;
    Ok(Json(tags))
}

struct TagTarget {
    package: Package,
    tag: String,
}

async fn writable_tag_target(
    state: &AppState,
    params: &HashMap<String, String>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<TagTarget> {
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;
    let repo_name = param(params, "repo")?;
    let package_name = extract_package_name(params);
    let tag = param(params, "tag")?;

    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Npm)?;
    crate::registry::ensure_can_write(&state.db, &repo, &user).await?;

    let package = crate::db::get_package(&state.db, repo.id, &package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;
    Ok(TagTarget {
        package,
        tag: tag.to_string(),
    })
}

pub async fn put_dist_tag(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth_user: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    // Body is the version string, JSON-encoded (e.g., "\"1.0.0\"")
    let version_str: String = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("invalid version string".to_string()))?;
    let target = writable_tag_target(&state, &params, auth_user).await?;

    let version = crate::db::get_version(&state.db, target.package.id, &version_str)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("version not found: {version_str}")))?;

    crate::db::set_dist_tag(&state.db, target.package.id, &target.tag, version.id).await?;

    Ok(Json(json!({"ok": true})))
}

pub async fn delete_dist_tag(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let target = writable_tag_target(&state, &params, auth_user).await?;

    sqlx::query("DELETE FROM dist_tags WHERE package_id = ?1 AND tag = ?2")
        .bind(target.package.id)
        .bind(target.tag.as_str())
        .execute(&state.db)
        .await?;

    Ok(Json(json!({"ok": true})))
}
