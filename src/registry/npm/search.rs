use std::collections::HashSet;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::middleware::AuthUser;
use crate::db::Package;
use crate::error::AppResult;
use crate::registry::resolve::collect;
use crate::server::AppState;

use super::cx;
use super::leaves::SearchLeaf;

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
) -> AppResult<Response> {
    let repo = crate::registry::load_repo(&state.db, &repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    // A negative `size` would become `LIMIT -1` (unlimited) in SQLite.
    let size = query.size.unwrap_or(20).clamp(0, 250);
    let from = query.from.unwrap_or(0).max(0);
    let leaf = SearchLeaf {
        text: query.text.unwrap_or_default(),
        limit: from + size,
    };
    let collected = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;

    let merged = dedup_by_name(collected.hits);
    let total = merged.len();
    let from_idx = (from as usize).min(total);
    let end_idx = (from_idx + size as usize).min(total);
    let mut response = Json(json!({
        "objects": &merged[from_idx..end_idx],
        "total": total,
        "time": "0ms",
    }))
    .into_response();
    if let Some(why) = collected.degraded {
        if let Ok(value) = HeaderValue::from_str(&format!("199 - \"{why}\"")) {
            response.headers_mut().insert(header::WARNING, value);
        }
    }
    Ok(response)
}

/// Member results in member order, the first occurrence of a name winning.
fn dedup_by_name(hits: Vec<Vec<Value>>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for obj in hits.into_iter().flatten() {
        let name = obj["package"]["name"].as_str().map(str::to_string);
        if name.is_none_or(|n| seen.insert(n)) {
            merged.push(obj);
        }
    }
    merged
}

/// The first `limit` search objects of one repository.
pub async fn search_in_repo(
    state: &AppState,
    repo_id: i64,
    text: &str,
    limit: i64,
) -> AppResult<Vec<Value>> {
    let packages = find_packages(state, repo_id, text, limit).await?;
    let mut objects = Vec::with_capacity(packages.len());
    for pkg in &packages {
        let versions = crate::db::get_versions(&state.db, pkg.id).await?;
        let dist_tags = crate::db::get_dist_tags(&state.db, pkg.id).await?;
        let latest = dist_tags
            .iter()
            .find(|dt| dt.tag == "latest")
            .and_then(|dt| versions.iter().find(|v| v.id == dt.version_id))
            .or_else(|| versions.last());
        objects.push(json!({
            "package": {
                "name": pkg.name,
                "description": pkg.description,
                "version": latest.map(|v| v.version.as_str()).unwrap_or("0.0.0"),
                "date": latest.map(|v| v.published_at.as_str()).unwrap_or(""),
            },
        }));
    }
    Ok(objects)
}

/// FTS5 first, LIKE when the query has no text or FTS refuses it.
async fn find_packages(
    state: &AppState,
    repo_id: i64,
    text: &str,
    limit: i64,
) -> AppResult<Vec<Package>> {
    if !text.is_empty() {
        let fts_query = text
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");
        let fts = sqlx::query_as::<_, Package>(
            "SELECT p.* FROM packages p \
             JOIN packages_fts fts ON p.id = fts.rowid \
             WHERE p.repository_id = ?1 AND packages_fts MATCH ?2 \
             ORDER BY rank \
             LIMIT ?3",
        )
        .bind(repo_id)
        .bind(&fts_query)
        .bind(limit)
        .fetch_all(&state.db)
        .await;
        if let Ok(packages) = fts {
            return Ok(packages);
        }
    }
    let pattern = format!("%{text}%");
    Ok(sqlx::query_as::<_, Package>(
        "SELECT * FROM packages WHERE repository_id = ?1 \
         AND (name LIKE ?2 OR description LIKE ?2) LIMIT ?3",
    )
    .bind(repo_id)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await?)
}
