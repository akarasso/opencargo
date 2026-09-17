use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::db::oci::OciTag;
use crate::error::AppResult;
use crate::server::AppState;

use super::OciRef;

#[derive(Deserialize)]
pub struct ListTagsQuery {
    n: Option<i64>,
    last: Option<String>,
}

pub async fn list_tags(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<ListTagsQuery>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    let r = OciRef::parse(&params)?;
    let repo = crate::registry::load_repo(&state.db, &r.repo).await?;
    crate::registry::ensure_can_read(&state.db, &repo, auth.as_ref().map(|e| &e.0)).await?;

    let limit = query.n.unwrap_or(100).min(10000);
    let tags: Vec<OciTag> = match &query.last {
        Some(last) => {
            sqlx::query_as(
                "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND tag > ?3 ORDER BY tag LIMIT ?4",
            )
            .bind(repo.id)
            .bind(&r.name)
            .bind(last)
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT * FROM oci_tags WHERE repository_id = ?1 AND name = ?2 ORDER BY tag LIMIT ?3",
            )
            .bind(repo.id)
            .bind(&r.name)
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
    };
    let tag_names: Vec<String> = tags.into_iter().map(|t| t.tag).collect();

    Ok(Json(json!({
        "name": r.image_name(),
        "tags": tag_names,
    }))
    .into_response())
}
