//! The subregistry read API: `/v0.1/servers`, a server's versions, and one
//! version, over every member the caller may read, each row gated by the
//! addressed repository and its member's floor.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use super::gate::Gates;
use super::schema::{MIRROR_META, OFFICIAL_META};
use crate::auth::middleware::AuthUser;
use crate::domain::governance::{Decision, Visibility};
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::mcp::{CatalogRow, PageQuery};
use crate::registry::cx;
use crate::registry::resolve::view;
use crate::server::AppState;

const DEFAULT_LIMIT: i64 = 30;
const MAX_LIMIT: i64 = 100;
const BATCH: i64 = 100;
const MAX_BATCHES: usize = 20;
const MAX_SEARCH_BATCHES: usize = 2;

pub async fn open(state: &AppState, name: &str, auth: Option<&AuthUser>) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_can_read(&*state.permissions, &repo, auth).await?;
    crate::registry::ensure_format(&repo, Format::Mcp)?;
    Ok(repo)
}

/// The members a read walks, in group order, and the gate over them.
pub async fn scope(state: &AppState, repo: &Repository, auth: Option<&AuthUser>) -> AppResult<(Vec<Repository>, Gates)> {
    let members = view(&cx(state, auth, repo), repo).await?;
    let gates = Gates::load(state.repos.as_ref(), state.mcp.as_ref(), &state.mcp_settings, repo).await?;
    Ok((members, gates))
}

fn include_deleted(params: &HashMap<String, String>) -> bool {
    match params.get("include_deleted").map(String::as_str) {
        Some("true") | Some("1") => true,
        Some(_) => false,
        None => params.contains_key("updated_since"),
    }
}

fn limit_of(params: &HashMap<String, String>) -> AppResult<i64> {
    match params.get("limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(raw) => raw
            .parse::<i64>()
            .map(|n| n.clamp(1, MAX_LIMIT))
            .map_err(|_| AppError::BadRequest(format!("invalid limit: {raw}"))),
    }
}

fn cursor_of(params: &HashMap<String, String>) -> AppResult<Option<(String, String)>> {
    let Some(raw) = params.get("cursor").filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    raw.split_once(':')
        .map(|(n, v)| Some((n.to_string(), v.to_string())))
        .ok_or_else(|| AppError::BadRequest(format!("invalid cursor: {raw}")))
}

fn since_of(params: &HashMap<String, String>) -> AppResult<Option<DateTime<Utc>>> {
    params
        .get("updated_since")
        .map(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|_| AppError::BadRequest(format!("invalid updated_since: {raw}")))
        })
        .transpose()
}

/// Our own key beside the official block, and on a hosted row the
/// official `isLatest` we mint; every other byte is the stored record's.
pub fn render(row: &CatalogRow, repo: &str, visibility: &Visibility) -> AppResult<Value> {
    let mut envelope: Value = serde_json::from_str(&row.envelope_json)?;
    let Some(obj) = envelope.as_object_mut() else {
        return Err(AppError::Internal("stored envelope is not an object".into()));
    };
    let meta = obj.entry("_meta").or_insert_with(|| Value::Object(Map::new()));
    let Some(meta) = meta.as_object_mut() else {
        return Err(AppError::Internal("stored _meta is not an object".into()));
    };
    if row.hosted {
        if let Some(official) = meta.get_mut(OFFICIAL_META).and_then(Value::as_object_mut) {
            official.insert("isLatest".into(), Value::Bool(row.is_latest));
        }
    }
    let approved = row.surface_endpoints > 0
        && row.approved_endpoints == row.surface_endpoints
        && row.worst_drift == crate::domain::Drift::None;
    let approval = match row.decision {
        Some(Decision::Blocked) => "blocked",
        _ if approved => "approved",
        _ => "pending",
    };
    meta.insert(
        MIRROR_META.into(),
        json!({
            "repository": repo,
            "approval": approval,
            "gate": if matches!(visibility, Visibility::Serve) { "serve" } else { "flagged" },
            "reason": visibility.reason(),
            "combinedSha256": row.current.as_ref().map(|c| c.combined_sha256.clone()),
            "toolsSha256": row.current.as_ref().and_then(|c| c.tools_sha256.clone()),
            "toolsSource": row.current.as_ref().map(|c| c.source.as_str()),
            "endpoints": {"approved": row.approved_endpoints, "total": row.surface_endpoints},
            "drift": row.worst_drift.as_str(),
            "findings": {"high": row.findings_high, "medium": row.findings_medium},
            "syncedAt": row.synced_at.to_rfc3339(),
        }),
    );
    Ok(envelope)
}

struct MemberPage {
    order: usize,
    survivors: Vec<(CatalogRow, Visibility)>,
    frontier: Option<(String, String)>,
    exhausted: bool,
}

/// Read one member forward from the cursor until `limit` rows survive the
/// gate or the member runs out, reporting how far it read.
async fn read_member(
    state: &AppState,
    gates: &Gates,
    member: &Repository,
    order: usize,
    base: &PageQuery,
    include_deleted: bool,
) -> AppResult<MemberPage> {
    let cap = if base.search.is_some() { MAX_SEARCH_BATCHES } else { MAX_BATCHES };
    let mut page = MemberPage {
        order,
        survivors: Vec::new(),
        frontier: None,
        exhausted: false,
    };
    let mut query = PageQuery {
        member: member.id,
        limit: BATCH,
        filters: gates.filters(member.id),
        require_approved: gates.require_approved(),
        ..base.clone()
    };
    for _ in 0..cap {
        let rows = state.mcp.page(&query).await?;
        let read = rows.len() as i64;
        for row in rows {
            page.frontier = Some((row.name.clone(), row.version.clone()));
            let visibility = gates.decide(member.id, &row, include_deleted);
            if visibility.served() {
                page.survivors.push((row, visibility));
            }
        }
        if read < BATCH {
            page.exhausted = true;
            break;
        }
        if page.survivors.len() as i64 >= base.limit {
            break;
        }
        query.after = page.frontier.clone();
    }
    Ok(page)
}

/// Merged in `(name, version)` order, the first member winning a
/// duplicate. A row past the frontier of a member that stopped early is
/// dropped, since that member may still hold rows before it; the cursor is
/// the last row emitted, and is present unless nothing was left unread.
fn merge(pages: Vec<MemberPage>, limit: i64) -> (Vec<(CatalogRow, Visibility)>, Option<String>) {
    let frontier = pages
        .iter()
        .filter(|p| !p.exhausted)
        .filter_map(|p| p.frontier.clone())
        .min();
    let all_exhausted = pages.iter().all(|p| p.exhausted);
    let mut rows: Vec<(usize, CatalogRow, Visibility)> = pages
        .into_iter()
        .flat_map(|p| {
            let order = p.order;
            p.survivors.into_iter().map(move |(r, v)| (order, r, v))
        })
        .collect();
    rows.sort_by(|a, b| (&a.1.name, &a.1.version, a.0).cmp(&(&b.1.name, &b.1.version, b.0)));
    rows.dedup_by(|b, a| a.1.name == b.1.name && a.1.version == b.1.version);
    let before = rows.len();
    if let Some((fname, fversion)) = &frontier {
        rows.retain(|(_, r, _)| (&r.name, &r.version) <= (fname, fversion));
    }
    let mut dropped = rows.len() < before;
    if rows.len() as i64 > limit {
        rows.truncate(limit as usize);
        dropped = true;
    }
    let cursor = match rows.last() {
        Some((_, r, _)) => Some(format!("{}:{}", r.name, r.version)),
        None => frontier.map(|(n, v)| format!("{n}:{v}")),
    };
    let cursor = if all_exhausted && !dropped { None } else { cursor };
    (rows.into_iter().map(|(_, r, v)| (r, v)).collect(), cursor)
}

pub async fn list_servers(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<Extension<AuthUser>>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let (members, gates) = scope(&state, &repo, auth).await?;
    let include_deleted = include_deleted(&params);
    let version = params.get("version").cloned();
    let base = PageQuery {
        addressed: repo.id,
        after: cursor_of(&params)?,
        limit: limit_of(&params)?,
        include_deleted,
        search: params.get("search").filter(|s| !s.is_empty()).cloned(),
        updated_since: since_of(&params)?,
        latest_only: version.as_deref() == Some("latest"),
        version: version.filter(|v| v != "latest"),
        ..PageQuery::default()
    };
    let mut pages = Vec::new();
    for (order, member) in members.iter().enumerate() {
        pages.push(read_member(&state, &gates, member, order, &base, include_deleted).await?);
    }
    let (rows, cursor) = merge(pages, base.limit);
    let servers = rows
        .iter()
        .map(|(row, visibility)| render(row, &repo.name, visibility))
        .collect::<AppResult<Vec<_>>>()?;
    let mut metadata = json!({"count": servers.len()});
    if let Some(cursor) = cursor {
        metadata["nextCursor"] = Value::String(cursor);
    }
    Ok(Json(json!({"servers": servers, "metadata": metadata})))
}

pub async fn list_versions(
    State(state): State<AppState>,
    Path((repo_name, server)): Path<(String, String)>,
    auth: Option<Extension<AuthUser>>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let (members, gates) = scope(&state, &repo, auth).await?;
    let include_deleted = include_deleted(&params);
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for member in &members {
        for row in state.mcp.versions_of(member.id, repo.id, &server, include_deleted).await? {
            let visibility = gates.decide(member.id, &row, include_deleted);
            if visibility.served() && seen.insert(row.version.clone()) {
                rows.push((row, visibility));
            }
        }
    }
    if rows.is_empty() {
        return Err(AppError::NotFound(format!("server not found: {server}")));
    }
    rows.sort_by(|a, b| (&b.0.published_at, &b.0.version).cmp(&(&a.0.published_at, &a.0.version)));
    let servers = rows
        .iter()
        .map(|(row, visibility)| render(row, &repo.name, visibility))
        .collect::<AppResult<Vec<_>>>()?;
    Ok(Json(json!({"servers": servers, "metadata": {"count": servers.len()}})))
}

/// The first member serving the version, and its gate.
pub async fn resolve_version(
    state: &AppState,
    repo: &Repository,
    auth: Option<&AuthUser>,
    server: &str,
    version: &str,
    include_deleted: bool,
) -> AppResult<(Repository, CatalogRow, Visibility, Gates)> {
    let (members, gates) = scope(state, repo, auth).await?;
    let wanted = (version != "latest").then_some(version);
    for member in members {
        if let Some(row) = state.mcp.version(member.id, repo.id, server, wanted).await? {
            let visibility = gates.decide(member.id, &row, include_deleted);
            if visibility.served() {
                return Ok((member, row, visibility, gates));
            }
        }
    }
    Err(AppError::NotFound(format!("server not found: {server}@{version}")))
}

pub async fn get_version(
    State(state): State<AppState>,
    Path((repo_name, server, version)): Path<(String, String, String)>,
    auth: Option<Extension<AuthUser>>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let include_deleted = include_deleted(&params);
    let (member, row, visibility, gates) = resolve_version(&state, &repo, auth, &server, &version, include_deleted).await?;
    super::record::served(&state, &repo, &member, &row, &gates, auth).await;
    Ok(Json(render(&row, &repo.name, &visibility)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Drift;

    fn row(name: &str) -> (CatalogRow, Visibility) {
        let (n, v) = name.split_once('@').unwrap();
        (
            CatalogRow {
                version_id: 0,
                member: 0,
                name: n.into(),
                version: v.into(),
                hosted: false,
                envelope_json: "{}".into(),
                status: "active".into(),
                is_latest: true,
                package_transports: String::new(),
                remote_transports: String::new(),
                published_at: None,
                synced_at: Utc::now(),
                current: None,
                findings_high: 0,
                findings_medium: 0,
                surface_endpoints: 0,
                approved_endpoints: 0,
                worst_drift: Drift::None,
                drifted_remote: None,
                decision: None,
                decided_at: None,
            },
            Visibility::Serve,
        )
    }

    fn page(order: usize, names: &[&str], frontier: Option<&str>, exhausted: bool) -> MemberPage {
        MemberPage {
            order,
            survivors: names.iter().map(|n| row(n)).collect(),
            frontier: frontier.map(|f| {
                let (n, v) = f.split_once('@').unwrap();
                (n.to_string(), v.to_string())
            }),
            exhausted,
        }
    }

    fn names(rows: &[(CatalogRow, Visibility)]) -> Vec<String> {
        rows.iter().map(|(r, _)| format!("{}@{}", r.name, r.version)).collect()
    }

    #[test]
    fn a_slow_member_holds_the_merged_cursor_at_its_own_frontier() {
        let fast = page(0, &["a/a@1", "a/b@1", "a/z@1"], Some("a/z@1"), true);
        let slow = page(1, &["a/c@1"], Some("a/d@1"), false);
        let (rows, cursor) = merge(vec![fast, slow], 30);
        assert_eq!(names(&rows), vec!["a/a@1", "a/b@1", "a/c@1"]);
        assert_eq!(cursor.as_deref(), Some("a/c:1"));
    }

    #[test]
    fn three_exhausted_members_truncated_to_the_limit_still_emit_a_cursor() {
        let pages = (0..3)
            .map(|m| {
                let rows: Vec<String> = (0..25).map(|i| format!("m{m}/s{i:02}@1")).collect();
                let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
                page(m, &refs, refs.last().copied(), true)
            })
            .collect();
        let (rows, cursor) = merge(pages, 30);
        assert_eq!(rows.len(), 30);
        assert_eq!(cursor.as_deref(), Some("m1/s04:1"));
    }

    #[test]
    fn duplicates_keep_the_first_member_and_an_empty_page_still_moves_forward() {
        let (rows, cursor) = merge(vec![page(0, &["a/x@1"], Some("a/x@1"), true), page(1, &["a/x@1"], Some("a/x@1"), true)], 30);
        assert_eq!(rows.len(), 1);
        assert_eq!(cursor, None);
        let (rows, cursor) = merge(vec![page(0, &[], Some("a/q@9"), false)], 30);
        assert!(rows.is_empty());
        assert_eq!(cursor.as_deref(), Some("a/q:9"));
    }
}
