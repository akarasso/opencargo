//! The reads of `McpStore`: a page is one indexed scan over rows that
//! already carry what it serves, joined to the addressed repository's
//! verdict and approval with the member's as the fallback.

use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::domain::governance::{AllowRule, Decision, Drift};
use crate::error::StoreError;
use crate::ports::mcp::{
    ApprovalRow, CatalogRow, CurrentSurface, FindingRow, NameFilter, PageQuery, ProbeRunRow, ProbeTarget, Subject,
    SurfaceSource, SyncState,
};

/// Every column of a `CatalogRow`; the addressed repository is bound first
/// in every query that uses it.
const SELECT: &str = "SELECT v.id, v.repository_id, v.name, v.version, v.hosted, v.response_json, v.status,
        v.is_latest, v.package_transports, v.remote_transports, v.published_at, v.synced_at,
        s.id AS s_id, s.source, s.remote_url, s.tools_sha256, s.permissions_sha256 AS s_permissions,
        s.combined_sha256,
        COALESCE(fc.high, v.findings_high) AS findings_high, COALESCE(fc.medium, v.findings_medium) AS findings_medium,
        CASE WHEN va.version_id IS NOT NULL THEN va.surface_endpoints ELSE COALESCE(vm.surface_endpoints, 0) END AS surface_endpoints,
        CASE WHEN va.version_id IS NOT NULL THEN va.approved_endpoints ELSE COALESCE(vm.approved_endpoints, 0) END AS approved_endpoints,
        CASE WHEN va.version_id IS NOT NULL THEN va.worst_drift ELSE COALESCE(vm.worst_drift, 'none') END AS worst_drift,
        CASE WHEN va.version_id IS NOT NULL THEN va.drifted_remote ELSE vm.drifted_remote END AS drifted_remote,
        COALESCE(aa.state, am.state) AS decision, COALESCE(aa.decided_at, am.decided_at) AS decided_at";

const JOINS: &str = " FROM mcp_server_versions v
        LEFT JOIN mcp_surfaces s ON s.id = v.current_surface_id
        LEFT JOIN mcp_version_verdicts va ON va.version_id = v.id AND va.repository_id = :addressed
        LEFT JOIN mcp_version_verdicts vm ON vm.version_id = v.id AND vm.repository_id = v.repository_id
        LEFT JOIN mcp_approvals aa ON aa.repository_id = :addressed AND aa.subject_kind = 'server'
            AND aa.name = v.name AND aa.version = v.version AND aa.remote_url = COALESCE(s.remote_url, '')
        LEFT JOIN mcp_approvals am ON am.repository_id = v.repository_id AND am.subject_kind = 'server'
            AND am.name = v.name AND am.version = v.version AND am.remote_url = COALESCE(s.remote_url, '')
        LEFT JOIN mcp_finding_counts fc ON fc.version_id = v.id AND fc.repository_id = :addressed";

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    repository_id: i64,
    name: String,
    version: String,
    hosted: bool,
    response_json: String,
    status: String,
    is_latest: bool,
    package_transports: String,
    remote_transports: String,
    published_at: Option<String>,
    synced_at: String,
    s_id: Option<i64>,
    source: Option<String>,
    remote_url: Option<String>,
    tools_sha256: Option<String>,
    s_permissions: Option<String>,
    combined_sha256: Option<String>,
    findings_high: i64,
    findings_medium: i64,
    surface_endpoints: i64,
    approved_endpoints: i64,
    worst_drift: String,
    drifted_remote: Option<String>,
    decision: Option<String>,
    decided_at: Option<String>,
}

fn catalog_row(r: Row) -> Result<CatalogRow, StoreError> {
    let current = match (r.s_id, r.source) {
        (Some(id), Some(source)) => Some(CurrentSurface {
            id,
            source: SurfaceSource::parse(&source).unwrap_or(SurfaceSource::Declared),
            remote_url: r.remote_url.unwrap_or_default(),
            tools_sha256: r.tools_sha256,
            permissions_sha256: r.s_permissions.unwrap_or_default(),
            combined_sha256: r.combined_sha256.unwrap_or_default(),
        }),
        _ => None,
    };
    let decided_at = r
        .decided_at
        .map(|at| read_ts(&r.name, "decided_at", &at))
        .transpose()
        .map_err(corrupt_row)?;
    Ok(CatalogRow {
        version_id: r.id,
        member: r.repository_id,
        synced_at: read_ts(&r.name, "synced_at", &r.synced_at).map_err(corrupt_row)?,
        name: r.name,
        version: r.version,
        hosted: r.hosted,
        envelope_json: r.response_json,
        status: r.status,
        is_latest: r.is_latest,
        package_transports: r.package_transports,
        remote_transports: r.remote_transports,
        published_at: r.published_at,
        current,
        findings_high: r.findings_high,
        findings_medium: r.findings_medium,
        surface_endpoints: r.surface_endpoints,
        approved_endpoints: r.approved_endpoints,
        worst_drift: r.worst_drift.parse().unwrap_or(Drift::None),
        drifted_remote: r.drifted_remote,
        decision: r.decision.and_then(|d| d.parse::<Decision>().ok()),
        decided_at,
    })
}

/// `name` in the half-open range a `*` rule covers: the prefix with its
/// last byte incremented bounds it, so the index serves the range.
fn push_rule(q: &mut QueryBuilder<'_, Sqlite>, rule: &AllowRule) {
    match rule.prefix() {
        Some(prefix) => {
            let mut upper = prefix.as_bytes().to_vec();
            if let Some(last) = upper.last_mut() {
                *last += 1;
            }
            q.push("(v.name >= ")
                .push_bind(prefix.to_string())
                .push(" AND v.name < ")
                .push_bind(String::from_utf8_lossy(&upper).into_owned())
                .push(")");
        }
        None => {
            q.push("v.name = ").push_bind(rule.pattern.clone());
        }
    }
}

fn push_filter(q: &mut QueryBuilder<'_, Sqlite>, filter: &NameFilter) {
    let allows: Vec<&AllowRule> = filter.allows().collect();
    if !allows.is_empty() {
        q.push(" AND (");
        for (i, rule) in allows.into_iter().enumerate() {
            if i > 0 {
                q.push(" OR ");
            }
            push_rule(q, rule);
        }
        q.push(")");
    }
    for deny in filter.subtractable_denies() {
        q.push(" AND NOT ");
        push_rule(q, deny);
    }
}

fn with_addressed(sql: &str, addressed: i64) -> String {
    sql.replace(":addressed", &addressed.to_string())
}

pub(super) async fn page(pool: &SqlitePool, p: &PageQuery) -> Result<Vec<CatalogRow>, StoreError> {
    let mut q: QueryBuilder<Sqlite> = QueryBuilder::new(with_addressed(&format!("{SELECT}{JOINS}"), p.addressed));
    q.push(" WHERE v.repository_id = ").push_bind(p.member);
    if let Some((name, version)) = &p.after {
        q.push(" AND (v.name > ")
            .push_bind(name.clone())
            .push(" OR (v.name = ")
            .push_bind(name.clone())
            .push(" AND v.version > ")
            .push_bind(version.clone())
            .push("))");
    }
    if !p.include_deleted {
        q.push(" AND v.status <> 'deleted'");
    }
    if let Some(search) = &p.search {
        q.push(" AND instr(v.name, ").push_bind(search.clone()).push(") > 0");
    }
    if let Some(since) = p.updated_since {
        q.push(" AND v.row_changed_at >= ").push_bind(bind_ts(since));
    }
    if p.latest_only {
        q.push(" AND v.is_latest = 1");
    }
    if let Some(version) = &p.version {
        q.push(" AND v.version = ").push_bind(version.clone());
    }
    for filter in &p.filters {
        push_filter(&mut q, filter);
    }
    if p.require_approved {
        q.push(
            " AND (CASE WHEN va.version_id IS NOT NULL
                   THEN va.surface_endpoints > 0 AND va.approved_endpoints = va.surface_endpoints AND va.worst_drift = 'none'
                   ELSE COALESCE(vm.surface_endpoints, 0) > 0 AND vm.approved_endpoints = vm.surface_endpoints
                        AND vm.worst_drift = 'none' END)",
        );
    }
    q.push(" ORDER BY v.name, v.version LIMIT ").push_bind(p.limit);
    let rows: Vec<Row> = q.build_query_as().fetch_all(pool).await.map_err(store_error)?;
    rows.into_iter().map(catalog_row).collect()
}

pub(super) async fn versions_of(
    pool: &SqlitePool,
    member: i64,
    addressed: i64,
    name: &str,
    include_deleted: bool,
) -> Result<Vec<CatalogRow>, StoreError> {
    let sql = with_addressed(
        &format!(
            "{SELECT}{JOINS} WHERE v.repository_id = ?1 AND v.name = ?2 AND (?3 OR v.status <> 'deleted')
             ORDER BY v.published_at DESC, v.version DESC"
        ),
        addressed,
    );
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(member)
        .bind(name)
        .bind(include_deleted)
        .fetch_all(pool)
        .await
        .map_err(store_error)?;
    rows.into_iter().map(catalog_row).collect()
}

pub(super) async fn version(
    pool: &SqlitePool,
    member: i64,
    addressed: i64,
    name: &str,
    version: Option<&str>,
) -> Result<Option<CatalogRow>, StoreError> {
    let sql = with_addressed(
        &format!(
            "{SELECT}{JOINS} WHERE v.repository_id = ?1 AND v.name = ?2
                 AND ((?3 IS NULL AND v.is_latest = 1) OR v.version = ?3)
             ORDER BY v.is_latest DESC LIMIT 1"
        ),
        addressed,
    );
    let row: Option<Row> = sqlx::query_as(&sql)
        .bind(member)
        .bind(name)
        .bind(version)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    row.map(catalog_row).transpose()
}

pub(super) async fn version_by_id(pool: &SqlitePool, id: i64, addressed: i64) -> Result<Option<CatalogRow>, StoreError> {
    let sql = with_addressed(&format!("{SELECT}{JOINS} WHERE v.id = ?1"), addressed);
    let row: Option<Row> = sqlx::query_as(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(store_error)?;
    row.map(catalog_row).transpose()
}

#[derive(sqlx::FromRow)]
struct FindingSql {
    id: i64,
    subject_kind: String,
    subject_id: i64,
    pattern: String,
    confidence: String,
    promoted_by: Option<String>,
    field: String,
    tool: String,
    span_start: i64,
    span_end: i64,
    excerpt: String,
    suppressed: bool,
}

pub(super) async fn findings_of(pool: &SqlitePool, version_id: i64, addressed: i64) -> Result<Vec<FindingRow>, StoreError> {
    let rows: Vec<FindingSql> = sqlx::query_as(
        "SELECT f.id, f.subject_kind, f.subject_id, f.pattern, f.confidence, f.promoted_by, f.field, f.tool,
                f.span_start, f.span_end, f.excerpt,
                EXISTS (SELECT 1 FROM mcp_suppressions x WHERE x.repository_id = ?2 AND x.pattern = f.pattern
                        AND (x.tool = '' OR x.tool = f.tool)) AS suppressed
         FROM mcp_findings f JOIN mcp_surfaces s ON f.subject_kind = 'surface' AND f.subject_id = s.id
         WHERE s.version_id = ?1 ORDER BY f.confidence DESC, f.id",
    )
    .bind(version_id)
    .bind(addressed)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    Ok(rows.into_iter().map(finding_row).collect())
}

fn finding_row(f: FindingSql) -> FindingRow {
    FindingRow {
        id: f.id,
        subject: if f.subject_kind == "skill" {
            Subject::Skill(f.subject_id)
        } else {
            Subject::Surface(f.subject_id)
        },
        pattern: f.pattern,
        high: f.confidence == "high",
        promoted_by: f.promoted_by,
        field: f.field,
        tool: f.tool,
        span: (f.span_start, f.span_end),
        excerpt: f.excerpt,
        suppressed: f.suppressed,
    }
}

#[derive(sqlx::FromRow)]
struct ApprovalSql {
    id: i64,
    repository_id: i64,
    name: String,
    version: String,
    remote_url: String,
    permissions_sha256: String,
    tools_sha256: Option<String>,
    surface_id: Option<i64>,
    state: String,
    decided_by: String,
    decided_at: String,
    note: Option<String>,
}

pub(super) async fn approvals_of(pool: &SqlitePool, name: &str, version: &str, skill: bool) -> Result<Vec<ApprovalRow>, StoreError> {
    let rows: Vec<ApprovalSql> = sqlx::query_as(
        "SELECT id, repository_id, name, version, remote_url, permissions_sha256, tools_sha256, surface_id, state,
                decided_by, decided_at, note
         FROM mcp_approvals WHERE subject_kind = ?1 AND name = ?2 AND version = ?3 ORDER BY repository_id, remote_url",
    )
    .bind(if skill { "skill" } else { "server" })
    .bind(name)
    .bind(version)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    rows.into_iter()
        .map(|a| {
            Ok(ApprovalRow {
                decision: a.state.parse().map_err(corrupt_row)?,
                decided_at: read_ts(&a.name, "decided_at", &a.decided_at).map_err(corrupt_row)?,
                id: a.id,
                repository: a.repository_id,
                name: a.name,
                version: a.version,
                remote_url: a.remote_url,
                permissions_sha256: a.permissions_sha256,
                tools_sha256: a.tools_sha256,
                surface_id: a.surface_id,
                decided_by: a.decided_by,
                note: a.note,
            })
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct RunSql {
    remote_url: String,
    ran_at: String,
    ok: bool,
    protocol_version: Option<String>,
    error: Option<String>,
}

pub(super) async fn probe_runs(pool: &SqlitePool, version_id: i64) -> Result<Vec<ProbeRunRow>, StoreError> {
    let rows: Vec<RunSql> = sqlx::query_as(
        "SELECT remote_url, ran_at, ok, protocol_version, error FROM mcp_probe_runs WHERE version_id = ?1
         ORDER BY ran_at DESC, id DESC",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    rows.into_iter()
        .map(|r| {
            Ok(ProbeRunRow {
                ran_at: read_ts(&r.remote_url, "ran_at", &r.ran_at).map_err(corrupt_row)?,
                remote_url: r.remote_url,
                ok: r.ok,
                protocol_version: r.protocol_version,
                error: r.error,
            })
        })
        .collect()
}

pub(super) async fn probe_targets(pool: &SqlitePool, repository: i64) -> Result<Vec<ProbeTarget>, StoreError> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, response_json FROM mcp_server_versions
         WHERE repository_id = ?1 AND is_latest = 1 AND status <> 'deleted' AND remote_urls <> '' ORDER BY name",
    )
    .bind(repository)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    let mut out = Vec::new();
    for (version_id, envelope_json) in rows {
        out.push(ProbeTarget {
            last_runs: probe_runs(pool, version_id).await?,
            version_id,
            envelope_json,
        });
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct SyncSql {
    high_water: Option<String>,
    last_full_at: Option<String>,
    last_run_at: Option<String>,
    last_error: Option<String>,
    skipped: i64,
    consecutive_failures: i64,
}

pub(super) async fn sync_state(pool: &SqlitePool, repository: i64) -> Result<SyncState, StoreError> {
    let row: Option<SyncSql> = sqlx::query_as(
        "SELECT high_water, last_full_at, last_run_at, last_error, skipped, consecutive_failures
         FROM mcp_sync_state WHERE repository_id = ?1",
    )
    .bind(repository)
    .fetch_optional(pool)
    .await
    .map_err(store_error)?;
    let Some(r) = row else {
        return Ok(SyncState::default());
    };
    let ts = |column: &'static str, v: Option<String>| {
        v.map(|at| read_ts("mcp_sync_state", column, &at)).transpose().map_err(corrupt_row)
    };
    Ok(SyncState {
        high_water: ts("high_water", r.high_water)?,
        last_full_at: ts("last_full_at", r.last_full_at)?,
        last_run_at: ts("last_run_at", r.last_run_at)?,
        last_error: r.last_error,
        skipped: r.skipped,
        consecutive_failures: r.consecutive_failures,
    })
}
