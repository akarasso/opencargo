//! `SearchQueryService`: hosted members answer through port 17, proxy
//! members through their upstream's search, and a group merges the hits by
//! id, the first member's winning.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::middleware::AuthUser;
use crate::domain::{CacheRepo, Outcome};
use crate::error::AppResult;
use crate::ports::nuget::FeedQuery;
use crate::registry::cx;
use crate::registry::resolve::{collect, Collected, Cx, Leaf, ResolveError, Upstream};
use crate::server::AppState;

use super::model::{self, Entry};
use super::read::{base, open};

pub const MAX_TAKE: u32 = 1000;
const DEFAULT_TAKE: u32 = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub skip: Option<u32>,
    #[serde(default)]
    pub take: Option<u32>,
    #[serde(default)]
    pub prerelease: Option<bool>,
    #[serde(default)]
    pub sem_ver_level: Option<String>,
    #[serde(default)]
    pub package_type: Option<String>,
}

impl SearchParams {
    pub fn skip(&self) -> u32 {
        self.skip.unwrap_or(0)
    }

    pub fn take(&self) -> u32 {
        self.take.unwrap_or(DEFAULT_TAKE).min(MAX_TAKE)
    }

    pub fn semver2(&self) -> bool {
        self.sem_ver_level
            .as_deref()
            .is_some_and(|l| l.starts_with('2'))
    }
}

/// One package of a search: its versions, oldest first, and downloads.
#[derive(Debug, Clone)]
pub struct Hit {
    pub key: String,
    pub entries: Vec<Entry>,
    pub downloads: i64,
}

/// What one member answers: its hits for the first `skip + take` rows and
/// its own total.
pub struct Found {
    pub total: u64,
    pub hits: Vec<Hit>,
}

pub struct SearchLeaf {
    pub params: SearchParams,
}

#[async_trait::async_trait]
impl Leaf for SearchLeaf {
    type Out = Found;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Found>, ResolveError> {
        let page = cx
            .nuget
            .search(&FeedQuery {
                repository: member.0.id,
                text: self.params.q.as_deref(),
                skip: 0,
                take: self.params.skip() + self.params.take(),
                prerelease: self.params.prerelease.unwrap_or(false),
                semver2: self.params.semver2(),
                package_type: self.params.package_type.as_deref(),
            })
            .await?;
        let hits = page
            .hits
            .into_iter()
            .map(|h| {
                let mut entries: Vec<Entry> =
                    h.versions.iter().map(|v| Entry::from_hosted(&h.package, v)).collect();
                model::sort(&mut entries);
                Hit {
                    key: h.package.name.clone(),
                    entries,
                    downloads: h.downloads,
                }
            })
            .collect();
        Ok(Outcome::Found(Found {
            total: page.total,
            hits,
        }))
    }

    async fn proxy(&self, cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> Result<Outcome<Found>, ResolveError> {
        super::upstream::search(cx, member, up, &self.params).await
    }
}

/// The members' hits merged by id; the total is the sum of the member
/// totals less the ids more than one member returned.
pub fn merge(found: Vec<Found>, skip: u32, take: u32) -> (u64, Vec<Hit>) {
    let mut total = 0u64;
    let mut hits: Vec<Hit> = Vec::new();
    for f in found {
        total += f.total;
        for h in f.hits {
            if hits.iter().any(|o| o.key == h.key) {
                total = total.saturating_sub(1);
            } else {
                hits.push(h);
            }
        }
    }
    if hits.len() > 1 {
        hits.sort_by(|a, b| a.key.cmp(&b.key));
    }
    let window = hits
        .into_iter()
        .skip(skip as usize)
        .take(take as usize)
        .collect();
    (total, window)
}

pub async fn search(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    Query(params): Query<SearchParams>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let (skip, take) = (params.skip(), params.take());
    let leaf = SearchLeaf { params };
    let Collected { hits, .. } = collect(&cx(&state, auth, &repo), &repo, &leaf).await?;
    let (total, window) = merge(hits, skip, take);
    let render = base(&state, &repo);
    let data: Vec<Value> = window
        .iter()
        .filter(|h| !h.entries.is_empty())
        .map(|h| render.search_hit(&h.entries, h.downloads))
        .collect();
    Ok(Json(json!({"totalHits": total, "data": data})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(key: &str) -> Hit {
        Hit {
            key: key.into(),
            entries: Vec::new(),
            downloads: 0,
        }
    }

    #[test]
    fn a_group_merges_by_id_and_cuts_its_own_window() {
        let (total, window) = merge(
            vec![
                Found { total: 2, hits: vec![hit("b"), hit("a")] },
                Found { total: 2, hits: vec![hit("a"), hit("c")] },
            ],
            1,
            5,
        );
        assert_eq!(total, 3);
        let keys: Vec<&str> = window.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, ["b", "c"]);
    }

    #[test]
    fn take_is_capped_and_semver_level_read() {
        let p = SearchParams {
            take: Some(5000),
            sem_ver_level: Some("2.0.0".into()),
            ..SearchParams::default()
        };
        assert_eq!(p.take(), MAX_TAKE);
        assert!(p.semver2());
        assert!(!SearchParams::default().semver2());
    }
}
