use std::collections::{BTreeSet, HashMap};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::collect;
use crate::server::AppState;

use super::leaves::TagsLeaf;
use super::{cx, OciRef};

const MAX_TAGS: usize = 10_000;

#[derive(Deserialize)]
pub struct ListTagsQuery {
    n: Option<i64>,
    last: Option<String>,
}

/// Union of every member's tags, sorted; `n`/`last` paginate the merged
/// list locally so a group pages like one registry.
pub async fn list_tags(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<ListTagsQuery>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let repo = crate::registry::load_repo(&state.db, &r.repo).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    let leaf = TagsLeaf {
        name: r.name.clone(),
    };
    let cx = cx(&state, auth, &repo);
    let collected = collect(&cx, &repo, &leaf).await?;
    let limit = query.n.unwrap_or(100).clamp(0, MAX_TAGS as i64) as usize;
    let tags: Vec<String> = collected
        .hits
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|tag| query.last.as_ref().is_none_or(|last| tag > last))
        .take(limit)
        .collect();

    let mut response = Json(json!({
        "name": format!("{}/{}", cx.url.0, r.name),
        "tags": tags,
    }))
    .into_response();
    if let Some(why) = collected.degraded {
        let warning = HeaderValue::from_str(&format!("199 - \"{}\"", why.replace('"', "'")))
            .map_err(|_| AppError::Internal("invalid warning header".into()))?;
        response.headers_mut().insert(header::WARNING, warning);
    }
    Ok(response)
}
