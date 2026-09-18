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
use crate::error::AppResult;
use crate::ports::search::{SearchQuery as Tokens, SearchScope};
use crate::registry::resolve::collect;
use crate::server::AppState;
use crate::wire::wire_ts;

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
    let repo = crate::registry::load_repo(state.repos.as_ref(), &repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;

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
    let Some(query) = browse_or_match(text) else {
        return Ok(Vec::new());
    };
    let packages = state
        .search
        .search(
            SearchScope::Repo(repo_id),
            query.as_ref(),
            limit.clamp(0, i64::from(u32::MAX)) as u32,
        )
        .await?;

    let mut objects = Vec::with_capacity(packages.len());
    for pkg in &packages {
        let versions = state.packages.versions(pkg.id).await?;
        let dist_tags = state.packages.dist_tags(pkg.id).await?;
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
                "date": latest.map(|v| wire_ts(v.published_at)).unwrap_or_default(),
            },
        }));
    }
    Ok(objects)
}

/// What the index is asked for, and the one case it is asked nothing.
///
/// No text at all is npm's *browse* — the call a client makes with no query,
/// which lists the repository — so it is `Some(None)`. A text that sanitises
/// away (`?q=%20`, `?q=%22`) matches nothing and never reaches the index,
/// which is what its `LIKE '% %'` predecessor amounted to and what keeps FTS5
/// from being handed an expression it refuses.
fn browse_or_match(text: &str) -> Option<Option<Tokens>> {
    if text.is_empty() {
        return Some(None);
    }
    Tokens::parse(text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_text_browses_and_an_unmatchable_one_asks_nothing() {
        assert!(browse_or_match("").is_some_and(|query| query.is_none()));
        assert!(
            browse_or_match("   ").is_none(),
            "whitespace alone matches nothing"
        );
        assert!(
            browse_or_match("\"").is_none(),
            "a lone quote sanitises away"
        );
        assert_eq!(
            browse_or_match("left-pad").unwrap().unwrap().tokens(),
            ["left-pad".to_string()]
        );
    }
}
