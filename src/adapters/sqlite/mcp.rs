//! `McpStore` over SQLite (023): the writes, and the one recomputation of
//! what a page serves that every write ends with.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, immediate, read_ts, corrupt_row, store_error, Tx};
use crate::domain::governance::{endpoint_verdict, AllowRule, Decision, Endpoint, EndpointVerdict, Fingerprint};
use crate::error::StoreError;
use crate::ports::mcp::{
    ApprovalRow, CatalogRow, FindingRow, McpStore, NewApproval, NewFinding, NewSurface, PageQuery, ProbeRun,
    ProbeRunRow, ProbeTarget, RecordWrite, StoredRule, Suppression, SurfaceRow, SurfaceSource, SyncState,
    Upserted,
};

#[path = "mcp_read.rs"]
mod read;
#[path = "mcp_skills.rs"]
mod skills;

pub(crate) use skills::REFERENCED;

pub struct SqliteMcpStore {
    pool: SqlitePool,
}

impl SqliteMcpStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn urls_of(stored: &str) -> Vec<String> {
    stored.lines().filter(|u| !u.is_empty()).map(str::to_string).collect()
}

async fn upsert_surface(tx: &mut Tx, version_id: i64, s: &NewSurface, ordinal: i64, now: DateTime<Utc>) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mcp_surfaces (version_id, source, remote_url, remote_ordinal, tools_json, tools_sha256,
                                   permissions_sha256, combined_sha256, captured_at, captured_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(version_id, source, remote_url, combined_sha256) DO UPDATE SET
             captured_at = excluded.captured_at, remote_ordinal = excluded.remote_ordinal
         RETURNING id",
    )
    .bind(version_id)
    .bind(s.source.as_str())
    .bind(&s.remote_url)
    .bind(ordinal)
    .bind(&s.tools_json)
    .bind(&s.tools_sha256)
    .bind(&s.permissions_sha256)
    .bind(&s.combined_sha256)
    .bind(bind_ts(now))
    .bind(&s.captured_by)
    .fetch_one(&mut **tx)
    .await?;
    replace_findings(tx, "surface", id, &s.findings).await?;
    Ok(id)
}

/// A subject's findings are written once: every scan replaces them.
async fn replace_findings(tx: &mut Tx, kind: &str, id: i64, findings: &[NewFinding]) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mcp_findings WHERE subject_kind = ?1 AND subject_id = ?2")
        .bind(kind)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    for f in findings {
        sqlx::query(
            "INSERT INTO mcp_findings (subject_kind, subject_id, pattern, confidence, promoted_by, field, tool,
                                       span_start, span_end, excerpt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(subject_kind, subject_id, pattern, field, tool, span_start) DO NOTHING",
        )
        .bind(kind)
        .bind(id)
        .bind(&f.pattern)
        .bind(if f.high { "high" } else { "medium" })
        .bind(&f.promoted_by)
        .bind(&f.field)
        .bind(&f.tool)
        .bind(f.span.0)
        .bind(f.span.1)
        .bind(&f.excerpt)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn ordinal_of(urls: &[String], url: &str) -> i64 {
    urls.iter().position(|u| u == url).map_or(0, |i| i as i64)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LiveSurface {
    id: i64,
    source: String,
    remote_url: String,
    tools_sha256: Option<String>,
    captured_at: String,
}

fn rank(source: &str) -> u8 {
    match SurfaceSource::parse(source) {
        Some(SurfaceSource::Attested) => 0,
        Some(SurfaceSource::Probe) => 1,
        _ => 2,
    }
}

/// The one precedence rule: attested over probe over declared, then the
/// record's declaration order, then the newest capture.
fn precedence(urls: &[String]) -> impl Fn(&LiveSurface, &LiveSurface) -> std::cmp::Ordering + '_ {
    move |a, b| {
        rank(&a.source)
            .cmp(&rank(&b.source))
            .then(ordinal_of(urls, &a.remote_url).cmp(&ordinal_of(urls, &b.remote_url)))
            .then(b.captured_at.cmp(&a.captured_at))
            .then(b.id.cmp(&a.id))
    }
}

#[derive(sqlx::FromRow)]
struct SurfaceSql {
    id: i64,
    source: String,
    remote_url: String,
    remote_ordinal: i64,
    tools_json: Option<String>,
    tools_sha256: Option<String>,
    permissions_sha256: String,
    combined_sha256: String,
    captured_at: String,
}

#[derive(sqlx::FromRow)]
struct VersionState {
    repository_id: i64,
    name: String,
    version: String,
    remote_urls: String,
    permissions_sha256: String,
    current_surface_id: Option<i64>,
    findings_high: i64,
    findings_medium: i64,
}

#[derive(sqlx::FromRow)]
struct ApprovalLite {
    id: i64,
    repository_id: i64,
    remote_url: String,
    permissions_sha256: String,
    tools_sha256: Option<String>,
    state: String,
}

/// Everything a page reads without aggregating, recomputed for one
/// version: the live surface set (one per source and endpoint, the rest
/// deleted with their findings unless an approval pins them), the current
/// surface, the counts over the live set, the endpoint set of the record,
/// and every interested repository's verdict and suppressed counts.
async fn refresh(tx: &mut Tx, version_id: i64, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let Some(v): Option<VersionState> = sqlx::query_as(
        "SELECT repository_id, name, version, remote_urls, permissions_sha256, current_surface_id,
                findings_high, findings_medium
         FROM mcp_server_versions WHERE id = ?1",
    )
    .bind(version_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(());
    };
    let urls = urls_of(&v.remote_urls);
    retire_withdrawn(tx, &v, &urls).await?;
    for (i, url) in urls.iter().enumerate() {
        sqlx::query("UPDATE mcp_surfaces SET remote_ordinal = ?1 WHERE version_id = ?2 AND remote_url = ?3")
            .bind(i as i64)
            .bind(version_id)
            .bind(url)
            .execute(&mut **tx)
            .await?;
    }
    let live = prune(tx, version_id, &urls).await?;
    let mut ordered = live.clone();
    ordered.sort_by(precedence(&urls));
    let current = ordered.first().map(|s| s.id);

    let live_ids: Vec<i64> = live.iter().map(|s| s.id).collect();
    let (high, medium) = counts(tx, &live_ids, None).await?;

    let observed: Vec<&LiveSurface> = ordered.iter().filter(|s| s.source != "declared").collect();
    let endpoint_urls: Vec<String> = if observed.is_empty() {
        vec![String::new()]
    } else {
        let mut seen = Vec::new();
        for s in &observed {
            if !seen.contains(&s.remote_url) {
                seen.push(s.remote_url.clone());
            }
        }
        seen
    };
    rekey_declared_slot(tx, &v, &endpoint_urls).await?;
    let current_of = |url: &str| Fingerprint {
        permissions: v.permissions_sha256.clone(),
        tools: ordered
            .iter()
            .find(|s| s.remote_url == url && (s.source != "declared" || observed.is_empty()))
            .and_then(|s| s.tools_sha256.clone()),
    };

    let approvals: Vec<ApprovalLite> = sqlx::query_as(
        "SELECT id, repository_id, remote_url, permissions_sha256, tools_sha256, state FROM mcp_approvals
         WHERE subject_kind = 'server' AND name = ?1 AND version = ?2",
    )
    .bind(&v.name)
    .bind(&v.version)
    .fetch_all(&mut **tx)
    .await?;
    let mut repos: BTreeSet<i64> = approvals.iter().map(|a| a.repository_id).collect();
    repos.insert(v.repository_id);
    let before: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "SELECT repository_id, surface_endpoints, approved_endpoints, worst_drift FROM mcp_version_verdicts
         WHERE version_id = ?1 ORDER BY repository_id",
    )
    .bind(version_id)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM mcp_version_verdicts WHERE version_id = ?1")
        .bind(version_id)
        .execute(&mut **tx)
        .await?;
    let mut after = Vec::new();
    for repo in repos {
        let endpoints: Vec<Endpoint> = endpoint_urls
            .iter()
            .map(|url| Endpoint {
                url: url.clone(),
                current: current_of(url),
                approval: approvals
                    .iter()
                    .find(|a| a.repository_id == repo && &a.remote_url == url)
                    .map(|a| {
                        let decision = a.state.parse().unwrap_or(Decision::Blocked);
                        let fp = Fingerprint {
                            permissions: a.permissions_sha256.clone(),
                            tools: a.tools_sha256.clone(),
                        };
                        (decision, fp)
                    }),
            })
            .collect();
        let verdict: EndpointVerdict = endpoint_verdict(&endpoints);
        write_verdict(tx, repo, version_id, &verdict).await?;
        after.push((repo, verdict.surface_endpoints, verdict.approved_endpoints, verdict.worst_drift.as_str().to_string()));
    }
    write_suppressed_counts(tx, version_id, &live_ids).await?;

    let moved = current != v.current_surface_id || high != v.findings_high || medium != v.findings_medium || before != after;
    sqlx::query(
        "UPDATE mcp_server_versions SET current_surface_id = ?1, findings_high = ?2, findings_medium = ?3,
             row_changed_at = CASE WHEN ?4 THEN ?5 ELSE row_changed_at END
         WHERE id = ?6",
    )
    .bind(current)
    .bind(high)
    .bind(medium)
    .bind(moved)
    .bind(bind_ts(now))
    .bind(version_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn write_verdict(tx: &mut Tx, repo: i64, version_id: i64, v: &EndpointVerdict) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mcp_version_verdicts (repository_id, version_id, surface_endpoints, approved_endpoints,
                                           worst_drift, drifted_remote)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(repo)
    .bind(version_id)
    .bind(v.surface_endpoints)
    .bind(v.approved_endpoints)
    .bind(v.worst_drift.as_str())
    .bind(&v.drifted_remote)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// An endpoint the record stopped declaring leaves the quorum: its
/// approvals are retired and its surfaces go with the pruning below.
async fn retire_withdrawn(tx: &mut Tx, v: &VersionState, urls: &[String]) -> Result<(), sqlx::Error> {
    let held: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, remote_url FROM mcp_approvals WHERE subject_kind = 'server' AND name = ?1 AND version = ?2
             AND remote_url <> ''",
    )
    .bind(&v.name)
    .bind(&v.version)
    .fetch_all(&mut **tx)
    .await?;
    for (id, url) in held {
        if !urls.contains(&url) {
            sqlx::query("DELETE FROM mcp_approvals WHERE id = ?1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

/// The declared slot is the endpoint of a version nobody observed; the
/// first observed url replaces it, carrying its approval with it.
async fn rekey_declared_slot(tx: &mut Tx, v: &VersionState, endpoints: &[String]) -> Result<(), sqlx::Error> {
    let Some(first) = endpoints.first().filter(|u| !u.is_empty()) else {
        return Ok(());
    };
    if endpoints.iter().any(String::is_empty) {
        return Ok(());
    }
    let slots: Vec<ApprovalLite> = sqlx::query_as(
        "SELECT id, repository_id, remote_url, permissions_sha256, tools_sha256, state FROM mcp_approvals
         WHERE subject_kind = 'server' AND name = ?1 AND version = ?2 AND remote_url = ''",
    )
    .bind(&v.name)
    .bind(&v.version)
    .fetch_all(&mut **tx)
    .await?;
    for slot in slots {
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM mcp_approvals WHERE repository_id = ?1 AND subject_kind = 'server'
                 AND name = ?2 AND version = ?3 AND remote_url = ?4",
        )
        .bind(slot.repository_id)
        .bind(&v.name)
        .bind(&v.version)
        .bind(first)
        .fetch_optional(&mut **tx)
        .await?;
        if taken.is_some() {
            sqlx::query("DELETE FROM mcp_approvals WHERE id = ?1")
                .bind(slot.id)
                .execute(&mut **tx)
                .await?;
        } else {
            sqlx::query("UPDATE mcp_approvals SET remote_url = ?1 WHERE id = ?2")
                .bind(first)
                .bind(slot.id)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

/// One surface per source and endpoint stays live; a superseded one, or
/// one for an endpoint the record withdrew, is deleted with its findings
/// unless an approval pins the bytes it was taken against.
async fn prune(tx: &mut Tx, version_id: i64, urls: &[String]) -> Result<Vec<LiveSurface>, sqlx::Error> {
    let all: Vec<LiveSurface> = sqlx::query_as(
        "SELECT id, source, remote_url, tools_sha256, captured_at FROM mcp_surfaces WHERE version_id = ?1
         ORDER BY captured_at DESC, id DESC",
    )
    .bind(version_id)
    .fetch_all(&mut **tx)
    .await?;
    let pinned: BTreeSet<i64> = sqlx::query_scalar(
        "SELECT surface_id FROM mcp_approvals WHERE surface_id IS NOT NULL
             AND surface_id IN (SELECT id FROM mcp_surfaces WHERE version_id = ?1)",
    )
    .bind(version_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let mut live: BTreeMap<(String, String), LiveSurface> = BTreeMap::new();
    let mut dead = Vec::new();
    for s in all {
        let withdrawn = !s.remote_url.is_empty() && !urls.contains(&s.remote_url);
        let key = (s.source.clone(), s.remote_url.clone());
        if withdrawn || live.contains_key(&key) {
            if withdrawn || !pinned.contains(&s.id) {
                dead.push(s.id);
            }
            continue;
        }
        live.insert(key, s);
    }
    for id in dead {
        sqlx::query("DELETE FROM mcp_findings WHERE subject_kind = 'surface' AND subject_id = ?1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM mcp_surfaces WHERE id = ?1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(live.into_values().collect())
}

/// High and medium findings over `surfaces`, less one repository's
/// suppressions when given.
async fn counts(tx: &mut Tx, surfaces: &[i64], suppressing: Option<i64>) -> Result<(i64, i64), sqlx::Error> {
    let (mut high, mut medium) = (0, 0);
    for &id in surfaces {
        let (h, m): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(f.confidence = 'high'), 0), COALESCE(SUM(f.confidence = 'medium'), 0)
             FROM mcp_findings f
             WHERE f.subject_kind = 'surface' AND f.subject_id = ?1
               AND (?2 IS NULL OR NOT EXISTS (SELECT 1 FROM mcp_suppressions s WHERE s.repository_id = ?2
                        AND s.pattern = f.pattern AND (s.tool = '' OR s.tool = f.tool)))",
        )
        .bind(id)
        .bind(suppressing)
        .fetch_one(&mut **tx)
        .await?;
        high += h;
        medium += m;
    }
    Ok((high, medium))
}

/// Sparse: a row only for the repositories holding a suppression.
async fn write_suppressed_counts(tx: &mut Tx, version_id: i64, live: &[i64]) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mcp_finding_counts WHERE version_id = ?1")
        .bind(version_id)
        .execute(&mut **tx)
        .await?;
    let repos: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT repository_id FROM mcp_suppressions")
        .fetch_all(&mut **tx)
        .await?;
    for repo in repos {
        let (high, medium) = counts(tx, live, Some(repo)).await?;
        sqlx::query("INSERT INTO mcp_finding_counts (repository_id, version_id, high, medium) VALUES (?1, ?2, ?3, ?4)")
            .bind(repo)
            .bind(version_id)
            .bind(high)
            .bind(medium)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// One repository's suppressed counts, rebuilt in two statements: a
/// per-version `refresh()` here would re-walk every mirrored record under
/// the single writer lock.
async fn refresh_repository_counts(tx: &mut Tx, repository: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mcp_finding_counts WHERE repository_id = ?1")
        .bind(repository)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO mcp_finding_counts (repository_id, version_id, high, medium)
         SELECT ?1, t.version_id,
                COALESCE(SUM(t.kept AND t.confidence = 'high'), 0),
                COALESCE(SUM(t.kept AND t.confidence = 'medium'), 0)
         FROM (SELECT s.version_id AS version_id, f.confidence AS confidence,
                      NOT EXISTS (SELECT 1 FROM mcp_suppressions sp WHERE sp.repository_id = ?1
                                    AND sp.pattern = f.pattern AND (sp.tool = '' OR sp.tool = f.tool)) AS kept
               FROM mcp_findings f JOIN mcp_surfaces s ON s.id = f.subject_id
               WHERE f.subject_kind = 'surface') t
         GROUP BY t.version_id
         HAVING SUM(NOT t.kept) > 0",
    )
    .bind(repository)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Rows a human wrote, which a repository deletion must not take: hosted
/// records and skills. Synced rows go with the repository.
pub(crate) async fn hosted_rows(tx: &mut Tx, repository: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM mcp_server_versions WHERE repository_id = ?1 AND hosted = 1)
              + (SELECT COUNT(*) FROM mcp_skills WHERE repository_id = ?1)",
    )
    .bind(repository)
    .fetch_one(&mut **tx)
    .await
}

/// The findings no foreign key takes with a repository's rows.
pub(crate) async fn forget_findings(tx: &mut Tx, repository: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM mcp_findings WHERE (subject_kind = 'surface' AND subject_id IN
             (SELECT s.id FROM mcp_surfaces s JOIN mcp_server_versions v ON v.id = s.version_id
              WHERE v.repository_id = ?1))
            OR (subject_kind = 'skill' AND subject_id IN (SELECT id FROM mcp_skills WHERE repository_id = ?1))",
    )
    .bind(repository)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_record(tx: &mut Tx, r: &RecordWrite) -> Result<Upserted, sqlx::Error> {
    let before: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, response_json FROM mcp_server_versions WHERE repository_id = ?1 AND name = ?2 AND version = ?3",
    )
    .bind(r.repository)
    .bind(&r.name)
    .bind(&r.version)
    .fetch_optional(&mut **tx)
    .await?;
    let now = bind_ts(r.now);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mcp_server_versions (repository_id, name, version, hosted, response_json, schema_url, status,
             status_message, status_changed_at, is_latest, package_transports, remote_transports, remote_urls,
             published_at, upstream_updated_at, synced_at, row_changed_at, permissions_sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16, ?17)
         ON CONFLICT(repository_id, name, version) DO UPDATE SET
             row_changed_at = CASE WHEN excluded.response_json <> mcp_server_versions.response_json
                                   THEN excluded.row_changed_at ELSE mcp_server_versions.row_changed_at END,
             hosted = excluded.hosted, response_json = excluded.response_json, schema_url = excluded.schema_url,
             status = excluded.status, status_message = excluded.status_message,
             status_changed_at = excluded.status_changed_at, is_latest = excluded.is_latest,
             package_transports = excluded.package_transports, remote_transports = excluded.remote_transports,
             remote_urls = excluded.remote_urls, published_at = excluded.published_at,
             upstream_updated_at = excluded.upstream_updated_at, synced_at = excluded.synced_at,
             permissions_sha256 = excluded.permissions_sha256
         RETURNING id",
    )
    .bind(r.repository)
    .bind(&r.name)
    .bind(&r.version)
    .bind(r.hosted)
    .bind(&r.envelope_json)
    .bind(&r.schema_url)
    .bind(&r.status)
    .bind(&r.status_message)
    .bind(&r.status_changed_at)
    .bind(r.is_latest)
    .bind(&r.package_transports)
    .bind(&r.remote_transports)
    .bind(r.remote_urls.join("\n"))
    .bind(&r.published_at)
    .bind(&r.upstream_updated_at)
    .bind(&now)
    .bind(&r.declared.permissions_sha256)
    .fetch_one(&mut **tx)
    .await?;
    if r.take_latest {
        sqlx::query(
            "UPDATE mcp_server_versions SET is_latest = 0, row_changed_at = ?1
             WHERE repository_id = ?2 AND name = ?3 AND id <> ?4 AND is_latest = 1",
        )
        .bind(&now)
        .bind(r.repository)
        .bind(&r.name)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    upsert_surface(tx, id, &r.declared, 0, r.now).await?;
    refresh(tx, id, r.now).await?;
    let changed = before.as_ref().is_none_or(|(_, json)| json != &r.envelope_json);
    Ok(Upserted { version_id: id, changed })
}

async fn record_probe(tx: &mut Tx, run: &ProbeRun) -> Result<Option<i64>, sqlx::Error> {
    let urls: Option<String> = sqlx::query_scalar("SELECT remote_urls FROM mcp_server_versions WHERE id = ?1")
        .bind(run.version_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(urls) = urls.map(|u| urls_of(&u)) else {
        return Ok(None);
    };
    let surface = match &run.outcome {
        Ok(s) => Some(upsert_surface(tx, run.version_id, s, ordinal_of(&urls, &s.remote_url), run.now).await?),
        Err(_) => None,
    };
    sqlx::query(
        "INSERT INTO mcp_probe_runs (version_id, remote_url, ran_at, ok, protocol_version, error, surface_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(run.version_id)
    .bind(&run.remote_url)
    .bind(bind_ts(run.now))
    .bind(surface.is_some())
    .bind(&run.protocol_version)
    .bind(run.outcome.as_ref().err())
    .bind(surface)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM mcp_probe_runs WHERE version_id = ?1 AND id NOT IN
             (SELECT id FROM mcp_probe_runs WHERE version_id = ?1 ORDER BY ran_at DESC, id DESC LIMIT 20)",
    )
    .bind(run.version_id)
    .execute(&mut **tx)
    .await?;
    refresh(tx, run.version_id, run.now).await?;
    Ok(surface)
}

fn rule_of(pattern: String, effect: String) -> Result<AllowRule, StoreError> {
    let effect = effect.parse().map_err(corrupt_row)?;
    Ok(AllowRule { pattern, effect })
}

async fn write_approval(tx: &mut Tx, a: &NewApproval) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO mcp_approvals (repository_id, subject_kind, name, version, permissions_sha256, tools_sha256,
             combined_sha256, remote_url, surface_id, state, decided_by, decided_at, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(repository_id, subject_kind, name, version, remote_url) DO UPDATE SET
             permissions_sha256 = excluded.permissions_sha256, tools_sha256 = excluded.tools_sha256,
             combined_sha256 = excluded.combined_sha256, surface_id = excluded.surface_id, state = excluded.state,
             decided_by = excluded.decided_by, decided_at = excluded.decided_at, note = excluded.note
         RETURNING id",
    )
    .bind(a.repository)
    .bind(if a.skill { "skill" } else { "server" })
    .bind(&a.name)
    .bind(&a.version)
    .bind(&a.permissions_sha256)
    .bind(&a.tools_sha256)
    .bind(&a.combined_sha256)
    .bind(&a.remote_url)
    .bind(a.surface_id)
    .bind(a.decision.as_str())
    .bind(&a.decided_by)
    .bind(bind_ts(a.now))
    .bind(&a.note)
    .fetch_one(&mut **tx)
    .await
}

#[async_trait]
impl McpStore for SqliteMcpStore {
    async fn publish_skill(&self, skill: &crate::ports::mcp::NewSkill) -> Result<i64, StoreError> {
        skills::publish(&self.pool, skill).await
    }

    async fn skills(&self, member: i64, addressed: i64) -> Result<Vec<crate::ports::mcp::SkillRow>, StoreError> {
        skills::list(&self.pool, member, addressed).await
    }

    async fn skill(&self, member: i64, addressed: i64, name: &str, version: &str) -> Result<Option<crate::ports::mcp::SkillRow>, StoreError> {
        skills::one(&self.pool, member, addressed, name, version).await
    }

    async fn delete_skill(&self, repository: i64, name: &str, version: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        skills::delete(&self.pool, repository, name, version, now).await
    }

    async fn upsert_record(&self, record: &RecordWrite) -> Result<Upserted, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = upsert_record(&mut tx, record).await.map(Ok);
                (tx, done)
            })
        })
        .await
    }

    async fn record_probe(&self, run: &ProbeRun) -> Result<Option<i64>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = record_probe(&mut tx, run).await.map(Ok);
                (tx, done)
            })
        })
        .await
    }

    async fn record_attested(&self, version_id: i64, surface: &NewSurface, now: DateTime<Utc>) -> Result<i64, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let urls: Option<String> =
                        sqlx::query_scalar("SELECT remote_urls FROM mcp_server_versions WHERE id = ?1")
                            .bind(version_id)
                            .fetch_optional(&mut *tx)
                            .await?;
                    let Some(urls) = urls.map(|u| urls_of(&u)) else {
                        return Ok(Err(StoreError::NotFound));
                    };
                    let id = upsert_surface(&mut tx, version_id, surface, ordinal_of(&urls, &surface.remote_url), now).await?;
                    refresh(&mut tx, version_id, now).await?;
                    Ok(Ok(id))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn decide(&self, approvals: &[NewApproval]) -> Result<Vec<i64>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let mut ids = Vec::new();
                    for a in approvals {
                        ids.push(write_approval(&mut tx, a).await?);
                        if !a.skill {
                            let versions: Vec<i64> = sqlx::query_scalar(
                                "SELECT id FROM mcp_server_versions WHERE name = ?1 AND version = ?2",
                            )
                            .bind(&a.name)
                            .bind(&a.version)
                            .fetch_all(&mut *tx)
                            .await?;
                            for id in versions {
                                refresh(&mut tx, id, a.now).await?;
                            }
                        }
                    }
                    Ok(Ok(ids))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn page(&self, query: &PageQuery) -> Result<Vec<CatalogRow>, StoreError> {
        read::page(&self.pool, query).await
    }

    async fn versions_of(&self, member: i64, addressed: i64, name: &str, include_deleted: bool) -> Result<Vec<CatalogRow>, StoreError> {
        read::versions_of(&self.pool, member, addressed, name, include_deleted).await
    }

    async fn version(&self, member: i64, addressed: i64, name: &str, version: Option<&str>) -> Result<Option<CatalogRow>, StoreError> {
        read::version(&self.pool, member, addressed, name, version).await
    }

    async fn version_by_id(&self, version_id: i64, addressed: i64) -> Result<Option<CatalogRow>, StoreError> {
        read::version_by_id(&self.pool, version_id, addressed).await
    }

    async fn surfaces(&self, version_id: i64) -> Result<Vec<SurfaceRow>, StoreError> {
        let urls: String = sqlx::query_scalar("SELECT remote_urls FROM mcp_server_versions WHERE id = ?1")
            .bind(version_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?
            .unwrap_or_default();
        let urls = urls_of(&urls);
        let rows: Vec<SurfaceSql> = sqlx::query_as(
            "SELECT id, source, remote_url, remote_ordinal, tools_json, tools_sha256, permissions_sha256,
                    combined_sha256, captured_at
             FROM mcp_surfaces WHERE version_id = ?1",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        let mut out = Vec::new();
        for s in rows {
            out.push(SurfaceRow {
                id: s.id,
                version_id,
                source: SurfaceSource::parse(&s.source).unwrap_or(SurfaceSource::Declared),
                remote_url: s.remote_url,
                remote_ordinal: s.remote_ordinal,
                tools_json: s.tools_json,
                tools_sha256: s.tools_sha256,
                permissions_sha256: s.permissions_sha256,
                combined_sha256: s.combined_sha256,
                captured_at: read_ts("mcp_surfaces", "captured_at", &s.captured_at).map_err(corrupt_row)?,
            });
        }
        let lite = |s: &SurfaceRow| LiveSurface {
            id: s.id,
            source: s.source.as_str().to_string(),
            remote_url: s.remote_url.clone(),
            tools_sha256: s.tools_sha256.clone(),
            captured_at: bind_ts(s.captured_at),
        };
        out.sort_by(|a, b| precedence(&urls)(&lite(a), &lite(b)));
        Ok(out)
    }

    async fn findings_of(&self, version_id: i64, addressed: i64) -> Result<Vec<FindingRow>, StoreError> {
        read::findings_of(&self.pool, version_id, addressed).await
    }

    async fn approvals_of(&self, name: &str, version: &str, skill: bool) -> Result<Vec<ApprovalRow>, StoreError> {
        read::approvals_of(&self.pool, name, version, skill).await
    }

    async fn probe_runs(&self, version_id: i64) -> Result<Vec<ProbeRunRow>, StoreError> {
        read::probe_runs(&self.pool, version_id).await
    }

    async fn probe_targets(&self, repository: i64) -> Result<Vec<ProbeTarget>, StoreError> {
        read::probe_targets(&self.pool, repository).await
    }

    async fn allow_rules(&self, repository: i64) -> Result<Vec<StoredRule>, StoreError> {
        let rows: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT id, pattern, effect FROM mcp_allow_rules WHERE repository_id = ?1 ORDER BY pattern")
                .bind(repository)
                .fetch_all(&self.pool)
                .await
                .map_err(store_error)?;
        rows.into_iter()
            .map(|(id, pattern, effect)| Ok(StoredRule { id, rule: rule_of(pattern, effect)? }))
            .collect()
    }

    async fn add_allow_rule(&self, repository: i64, rule: &AllowRule, by: Option<&str>, now: DateTime<Utc>) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "INSERT INTO mcp_allow_rules (repository_id, pattern, effect, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
        )
        .bind(repository)
        .bind(&rule.pattern)
        .bind(rule.effect.as_str())
        .bind(by)
        .bind(bind_ts(now))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn delete_allow_rule(&self, repository: i64, id: i64) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM mcp_allow_rules WHERE repository_id = ?1 AND id = ?2")
            .bind(repository)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn suppressions(&self, repository: i64) -> Result<Vec<Suppression>, StoreError> {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, pattern, tool FROM mcp_suppressions WHERE repository_id = ?1 ORDER BY pattern, tool",
        )
        .bind(repository)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(|(id, pattern, tool)| Suppression { id, pattern, tool }).collect())
    }

    async fn add_suppression(&self, repository: i64, pattern: &str, tool: &str, by: Option<&str>, now: DateTime<Utc>) -> Result<i64, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let id: i64 = sqlx::query_scalar(
                        "INSERT INTO mcp_suppressions (repository_id, pattern, tool, created_by, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                    )
                    .bind(repository)
                    .bind(pattern)
                    .bind(tool)
                    .bind(by)
                    .bind(bind_ts(now))
                    .fetch_one(&mut *tx)
                    .await?;
                    refresh_repository_counts(&mut tx, repository).await?;
                    Ok(Ok(id))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn delete_suppression(&self, repository: i64, id: i64, _now: DateTime<Utc>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let gone = sqlx::query("DELETE FROM mcp_suppressions WHERE repository_id = ?1 AND id = ?2")
                        .bind(repository)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    if gone.rows_affected() == 0 {
                        return Ok(Err(StoreError::NotFound));
                    }
                    refresh_repository_counts(&mut tx, repository).await?;
                    Ok(Ok(()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn seed(&self, repository: i64, rules: &[AllowRule], suppressions: &[(String, String)], now: DateTime<Utc>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    for rule in rules {
                        sqlx::query(
                            "INSERT INTO mcp_allow_rules (repository_id, pattern, effect, created_by, created_at)
                             VALUES (?1, ?2, ?3, 'config', ?4) ON CONFLICT(repository_id, pattern) DO NOTHING",
                        )
                        .bind(repository)
                        .bind(&rule.pattern)
                        .bind(rule.effect.as_str())
                        .bind(bind_ts(now))
                        .execute(&mut *tx)
                        .await?;
                    }
                    let mut added = false;
                    for (pattern, tool) in suppressions {
                        let done = sqlx::query(
                            "INSERT INTO mcp_suppressions (repository_id, pattern, tool, created_by, created_at)
                             VALUES (?1, ?2, ?3, 'config', ?4) ON CONFLICT(repository_id, pattern, tool) DO NOTHING",
                        )
                        .bind(repository)
                        .bind(pattern)
                        .bind(tool)
                        .bind(bind_ts(now))
                        .execute(&mut *tx)
                        .await?;
                        added |= done.rows_affected() > 0;
                    }
                    if added {
                        refresh_repository_counts(&mut tx, repository).await?;
                    }
                    Ok(Ok(()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn sync_state(&self, repository: i64) -> Result<SyncState, StoreError> {
        read::sync_state(&self.pool, repository).await
    }

    async fn save_sync_state(&self, repository: i64, state: &SyncState) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO mcp_sync_state (repository_id, high_water, last_full_at, last_run_at, last_error, skipped,
                                         consecutive_failures)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(repository_id) DO UPDATE SET high_water = excluded.high_water,
                 last_full_at = excluded.last_full_at, last_run_at = excluded.last_run_at,
                 last_error = excluded.last_error, skipped = excluded.skipped,
                 consecutive_failures = excluded.consecutive_failures",
        )
        .bind(repository)
        .bind(state.high_water.map(bind_ts))
        .bind(state.last_full_at.map(bind_ts))
        .bind(state.last_run_at.map(bind_ts))
        .bind(&state.last_error)
        .bind(state.skipped)
        .bind(state.consecutive_failures)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn hosted_count(&self, repository: i64) -> Result<i64, StoreError> {
        let mut conn = self.pool.begin().await.map_err(store_error)?;
        let n = hosted_rows(&mut conn, repository).await.map_err(store_error)?;
        conn.rollback().await.map_err(store_error)?;
        Ok(n)
    }

    async fn purge(&self, repository: i64) -> Result<u64, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    sqlx::query(
                        "DELETE FROM mcp_findings WHERE subject_kind = 'surface' AND subject_id IN
                             (SELECT s.id FROM mcp_surfaces s JOIN mcp_server_versions v ON v.id = s.version_id
                              WHERE v.repository_id = ?1)",
                    )
                    .bind(repository)
                    .execute(&mut *tx)
                    .await?;
                    let gone = sqlx::query("DELETE FROM mcp_server_versions WHERE repository_id = ?1 AND hosted = 0")
                        .bind(repository)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("DELETE FROM mcp_sync_state WHERE repository_id = ?1")
                        .bind(repository)
                        .execute(&mut *tx)
                        .await?;
                    Ok(Ok(gone.rows_affected()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
