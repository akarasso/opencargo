use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::middleware::AuthUser;
use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::proxy;
use crate::registry::extract_package_name;
use crate::registry::resolve::{first_hit, Cx, FailurePolicy, UrlRepo};
use crate::server::AppState;

use super::leaves::{PackumentLeaf, TarballLeaf};
use super::param;

const ABBREVIATED_TYPE: &str = "application/vnd.npm.install-v1+json";

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

fn cx<'a>(state: &'a AppState, auth: Option<&'a AuthUser>, repo: &'a Repository) -> Cx<'a> {
    Cx {
        state,
        auth,
        url: UrlRepo(&repo.name),
        failure: FailurePolicy::NotFound,
    }
}

pub async fn get_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let repo_name = param(&params, "repo")?;
    let package_name = extract_package_name(&params);
    crate::registry::validate_package_name("npm", &package_name)?;

    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    let abbreviated = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(ABBREVIATED_TYPE));
    let leaf = PackumentLeaf {
        name: package_name.clone(),
        abbreviated,
    };
    let cx = cx(&state, auth, &repo);
    let mut packument = first_hit(&cx, &repo, &leaf).await?;
    proxy::rewrite_tarball_urls(&mut packument.json, &state.base_url, cx.url.0, &package_name);

    let mut response = Json(packument.json).into_response();
    if abbreviated {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ABBREVIATED_TYPE));
    }
    if packument.stale {
        response.headers_mut().insert(
            header::WARNING,
            HeaderValue::from_static("110 - \"Response is Stale\""),
        );
    }
    Ok(response)
}

pub async fn download_tarball(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let repo_name = param(&params, "repo")?;
    let package_name = extract_package_name(&params);
    let filename = param(&params, "filename")?;
    crate::registry::validate_package_name("npm", &package_name)?;
    validate_tarball_filename(filename)?;

    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

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

#[derive(Deserialize)]
pub struct SearchQuery {
    text: Option<String>,
    size: Option<i64>,
    from: Option<i64>,
}

pub async fn search(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    Query(query): Query<SearchQuery>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::registry::load_repo(&state.db, &repo_name).await?;

    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    let search_text = query.text.unwrap_or_default();
    // Clamp both bounds: a negative `size` would become `LIMIT -1` (unlimited)
    // in SQLite, bypassing the 250 cap and amplifying the per-row N+1 lookups;
    // a negative `from` would be an invalid OFFSET.
    let size = query.size.unwrap_or(20).clamp(0, 250);
    let from = query.from.unwrap_or(0).max(0);

    match repo.kind()? {
        crate::db::kinds::RepoKind::Group => {
            // Merge search results from all member repos
            let members = repo.members();
            let mut all_objects = Vec::new();
            let mut seen_names = std::collections::HashSet::new();

            for member_name in &members {
                let member_repo =
                    match crate::db::get_repository_by_name(&state.db, member_name).await? {
                        Some(r) => r,
                        None => continue,
                    };

                // Fetch the first (from + size) results from each member,
                // starting at offset 0 — NOT (size, from). The merged result is
                // re-paginated below, so passing `from` here skipped it twice and
                // dropped results for from > 0.
                if crate::registry::ensure_can_read(&state.db, &member_repo, auth.as_ref().map(|e| &e.0)).await.is_err() {
                    continue;
                }
                let member_objects =
                    search_in_repo(&state, member_repo.id, &search_text, from + size, 0).await?;

                for obj in member_objects {
                    // Deduplicate by package name
                    if let Some(name) = obj
                        .get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        if seen_names.insert(name.to_string()) {
                            all_objects.push(obj);
                        }
                    } else {
                        all_objects.push(obj);
                    }
                }
            }

            // Apply pagination to merged results
            let total = all_objects.len();
            let from_idx = (from as usize).min(total);
            let end_idx = (from_idx + size as usize).min(total);
            let page = &all_objects[from_idx..end_idx];

            Ok(Json(json!({
                "objects": page,
                "total": total,
                "time": "0ms",
            })))
        }
        crate::db::kinds::RepoKind::Hosted | crate::db::kinds::RepoKind::Proxy => {
            let objects = search_in_repo(&state, repo.id, &search_text, size, from).await?;
            let total = objects.len();

            Ok(Json(json!({
                "objects": objects,
                "total": total,
                "time": "0ms",
            })))
        }
    }
}

/// Search for packages in a single repository by ID.
///
/// Uses FTS5 full-text search when available, falling back to LIKE queries
/// if the FTS query fails (e.g., special characters or FTS5 not available).
async fn search_in_repo(
    state: &AppState,
    repo_id: i64,
    search_text: &str,
    size: i64,
    from: i64,
) -> Result<Vec<Value>, AppError> {
    // Try FTS5 first
    let packages: Vec<crate::db::Package> = if !search_text.is_empty() {
        // Sanitize the search text for FTS5: wrap each word in double quotes to avoid
        // syntax errors from special characters
        let fts_query = search_text
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");

        let fts_result: Result<Vec<crate::db::Package>, _> = sqlx::query_as(
            "SELECT p.* FROM packages p \
             JOIN packages_fts fts ON p.id = fts.rowid \
             WHERE p.repository_id = ?1 AND packages_fts MATCH ?2 \
             ORDER BY rank \
             LIMIT ?3 OFFSET ?4",
        )
        .bind(repo_id)
        .bind(&fts_query)
        .bind(size)
        .bind(from)
        .fetch_all(&state.db)
        .await;

        match fts_result {
            Ok(pkgs) => pkgs,
            Err(_) => {
                // Fallback to LIKE if FTS query fails
                let pattern = format!("%{search_text}%");
                sqlx::query_as(
                    "SELECT * FROM packages WHERE repository_id = ?1 AND (name LIKE ?2 OR description LIKE ?2) LIMIT ?3 OFFSET ?4",
                )
                .bind(repo_id)
                .bind(&pattern)
                .bind(size)
                .bind(from)
                .fetch_all(&state.db)
                .await?
            }
        }
    } else {
        let pattern = format!("%{search_text}%");
        sqlx::query_as(
            "SELECT * FROM packages WHERE repository_id = ?1 AND (name LIKE ?2 OR description LIKE ?2) LIMIT ?3 OFFSET ?4",
        )
        .bind(repo_id)
        .bind(&pattern)
        .bind(size)
        .bind(from)
        .fetch_all(&state.db)
        .await?
    };

    let mut objects = Vec::new();
    for pkg in &packages {
        let dist_tags = crate::db::get_dist_tags(&state.db, pkg.id).await?;
        let versions = crate::db::get_versions(&state.db, pkg.id).await?;

        let latest_version = dist_tags
            .iter()
            .find(|dt| dt.tag == "latest")
            .and_then(|dt| versions.iter().find(|v| v.id == dt.version_id))
            .or_else(|| versions.last());

        objects.push(json!({
            "package": {
                "name": pkg.name,
                "description": pkg.description,
                "version": latest_version.map(|v| v.version.as_str()).unwrap_or("0.0.0"),
                "date": latest_version.map(|v| v.published_at.as_str()).unwrap_or(""),
            },
        }));
    }

    Ok(objects)
}
