use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{Resolution, RuleVerdict, Verdict};

/// One transaction for a whole batch; verdicts follow their resolution.
pub async fn insert_batch(
    pool: &SqlitePool,
    rows: &[(Resolution, Vec<RuleVerdict>)],
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut ids = Vec::with_capacity(rows.len());
    for (r, verdicts) in rows {
        let would_block = verdicts.iter().any(|v| v.verdict == Verdict::WouldBlock);
        let unknown = !would_block && verdicts.iter().any(|v| v.verdict == Verdict::Unknown);
        let id = sqlx::query(
            "INSERT INTO policy_resolutions
                 (requested_repo, member_repo, format, name, version, digest, published_at,
                  date_source, actor, actor_kind, user_id, would_block, unknown)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(&r.requested_repo)
        .bind(&r.member_repo)
        .bind(r.format.as_str())
        .bind(&r.name)
        .bind(&r.version)
        .bind(&r.digest)
        .bind(
            r.published_at
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
        )
        .bind(r.facts.date_source)
        .bind(&r.actor.name)
        .bind(r.actor.kind.as_str())
        .bind(r.actor.user_id)
        .bind(would_block)
        .bind(unknown)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        for v in verdicts {
            sqlx::query(
                "INSERT INTO policy_verdicts (resolution_id, rule, verdict, reason)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(id)
            .bind(v.rule)
            .bind(v.verdict.as_str())
            .bind(&v.reason)
            .execute(&mut *tx)
            .await?;
        }
        ids.push(id);
    }
    tx.commit().await?;
    Ok(ids)
}

/// Whose rows `/me/policy` shows: a DB user's, or the config token's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    User(i64),
    Static,
}

/// `repo` matches the requested or the member repository; `rule` narrows
/// every number and verdict to that rule's own row.
#[derive(Debug, Clone, Copy)]
pub struct ReportFilter<'a> {
    pub since: DateTime<Utc>,
    pub repo: Option<&'a str>,
    pub rule: Option<&'a str>,
    pub subject: Option<Subject>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RuleTotals {
    pub would_block: u64,
    pub unknown: u64,
    pub pass: u64,
    pub not_applicable: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub resolutions: u64,
    pub would_block: u64,
    pub unknown: u64,
    pub by_rule: BTreeMap<String, RuleTotals>,
}

impl Totals {
    /// Adds the counts of a later id range.
    pub fn absorb(&mut self, delta: Totals) {
        self.resolutions += delta.resolutions;
        self.would_block += delta.would_block;
        self.unknown += delta.unknown;
        for (rule, d) in delta.by_rule {
            let t = self.by_rule.entry(rule).or_default();
            t.would_block += d.would_block;
            t.unknown += d.unknown;
            t.pass += d.pass;
            t.not_applicable += d.not_applicable;
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ResolutionRow {
    pub id: i64,
    pub created_at: String,
    pub requested_repo: String,
    pub member_repo: String,
    pub format: String,
    pub name: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub actor: String,
    pub actor_kind: String,
    pub user_id: Option<i64>,
    pub published_at: Option<String>,
    pub would_block: bool,
    pub unknown: bool,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct VerdictRow {
    #[serde(skip)]
    pub resolution_id: i64,
    pub rule: String,
    pub verdict: String,
    pub reason: String,
}

#[derive(sqlx::FromRow)]
struct TotalsRow {
    resolutions: i64,
    would_block: i64,
    unknown: i64,
}

#[derive(sqlx::FromRow)]
struct RuleCountRow {
    rule: String,
    verdict: String,
    count: i64,
}

fn sqlite_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Which rows of the window the totals cover: `after < id <= upto`, so
/// a snapshot and the delta on top of it never count a row twice.
#[derive(Debug, Clone, Copy)]
pub struct IdRange {
    pub after: i64,
    pub upto: i64,
}

/// `FROM policy_resolutions r [JOIN that rule's verdict v]`, the scan
/// every report query shares.
fn from<'a>(q: &mut QueryBuilder<'a, Sqlite>, f: &ReportFilter<'a>) {
    q.push(" FROM policy_resolutions r");
    if let Some(rule) = f.rule {
        q.push(" JOIN policy_verdicts v ON v.resolution_id = r.id AND v.rule = ");
        q.push_bind(rule);
    }
}

fn filters<'a>(q: &mut QueryBuilder<'a, Sqlite>, f: &ReportFilter<'a>, range: Option<IdRange>) {
    q.push(" WHERE r.created_at >= ");
    q.push_bind(sqlite_time(f.since));
    if let Some(range) = range {
        q.push(" AND r.id > ");
        q.push_bind(range.after);
        q.push(" AND r.id <= ");
        q.push_bind(range.upto);
    }
    if let Some(repo) = f.repo {
        q.push(" AND (r.requested_repo = ");
        q.push_bind(repo);
        q.push(" OR r.member_repo = ");
        q.push_bind(repo);
        q.push(")");
    }
    match f.subject {
        Some(Subject::User(id)) => {
            q.push(" AND r.user_id = ");
            q.push_bind(id);
        }
        Some(Subject::Static) => {
            q.push(" AND r.actor_kind = 'static'");
        }
        None => {}
    }
}

fn flags(f: &ReportFilter<'_>) -> &'static str {
    match f.rule {
        Some(_) => "v.verdict = 'would_block' AS would_block, v.verdict = 'unknown' AS unknown",
        None => "r.would_block, r.unknown",
    }
}

/// The newest resolution id, the upper bound of a totals snapshot.
pub async fn max_id(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM policy_resolutions")
        .fetch_one(pool)
        .await
}

/// Totals over the rows of `range` in the window: one covering index scan
/// for the head, one join for the per-rule counts.
pub async fn report_totals(
    pool: &SqlitePool,
    f: &ReportFilter<'_>,
    range: IdRange,
) -> Result<Totals, sqlx::Error> {
    let mut q = QueryBuilder::new("SELECT COUNT(*) AS resolutions, ");
    q.push(match f.rule {
        Some(_) => "COALESCE(SUM(v.verdict = 'would_block'), 0) AS would_block, COALESCE(SUM(v.verdict = 'unknown'), 0) AS unknown",
        None => "COALESCE(SUM(r.would_block), 0) AS would_block, COALESCE(SUM(r.unknown), 0) AS unknown",
    });
    from(&mut q, f);
    filters(&mut q, f, Some(range));
    let head: TotalsRow = q.build_query_as().fetch_one(pool).await?;

    let verdicts = if f.rule.is_some() { "v" } else { "pv" };
    let mut q = QueryBuilder::new(format!(
        "SELECT {verdicts}.rule, {verdicts}.verdict, COUNT(*) AS count"
    ));
    from(&mut q, f);
    if f.rule.is_none() {
        q.push(" JOIN policy_verdicts pv ON pv.resolution_id = r.id");
    }
    filters(&mut q, f, Some(range));
    q.push(" GROUP BY 1, 2");
    let counts: Vec<RuleCountRow> = q.build_query_as().fetch_all(pool).await?;

    let mut by_rule: BTreeMap<String, RuleTotals> = BTreeMap::new();
    for c in counts {
        let entry = by_rule.entry(c.rule).or_default();
        let n = c.count as u64;
        match c.verdict.as_str() {
            "would_block" => entry.would_block += n,
            "unknown" => entry.unknown += n,
            "pass" => entry.pass += n,
            _ => entry.not_applicable += n,
        }
    }
    Ok(Totals {
        resolutions: head.resolutions as u64,
        would_block: head.would_block as u64,
        unknown: head.unknown as u64,
        by_rule,
    })
}

/// Newest first; `created_at` comes back as RFC 3339.
pub async fn list_resolutions(
    pool: &SqlitePool,
    f: &ReportFilter<'_>,
    page: i64,
    size: i64,
) -> Result<Vec<ResolutionRow>, sqlx::Error> {
    let mut q = QueryBuilder::new(
        "SELECT r.id, strftime('%Y-%m-%dT%H:%M:%SZ', r.created_at) AS created_at, r.requested_repo, r.member_repo,
                r.format, r.name, r.version, r.digest, r.actor, r.actor_kind, r.user_id, r.published_at, ",
    );
    q.push(flags(f));
    from(&mut q, f);
    filters(&mut q, f, None);
    q.push(" ORDER BY r.id DESC LIMIT ");
    q.push_bind(size);
    q.push(" OFFSET ");
    q.push_bind(page.saturating_sub(1).max(0).saturating_mul(size.max(0)));
    q.build_query_as().fetch_all(pool).await
}

/// The verdicts of the page's rows, that rule's alone when `rule` is set.
pub async fn verdicts_for(
    pool: &SqlitePool,
    ids: &[i64],
    rule: Option<&str>,
) -> Result<Vec<VerdictRow>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut q = QueryBuilder::new(
        "SELECT resolution_id, rule, verdict, reason FROM policy_verdicts WHERE resolution_id IN (",
    );
    let mut list = q.separated(", ");
    for id in ids {
        list.push_bind(*id);
    }
    q.push(")");
    if let Some(rule) = rule {
        q.push(" AND rule = ");
        q.push_bind(rule);
    }
    q.push(" ORDER BY resolution_id, rule");
    q.build_query_as().fetch_all(pool).await
}

/// Rows deleted per transaction: the write lock is held for one chunk,
/// never for a whole day of resolutions and their verdicts.
pub const DELETE_CHUNK: i64 = 5000;

/// Retention: rows older than `days` go, chunk by chunk.
pub async fn delete_older_than(pool: &SqlitePool, days: u64) -> Result<u64, sqlx::Error> {
    delete_where(
        pool,
        "created_at < datetime('now', '-' || ?1 || ' days')",
        days as i64,
    )
    .await
}

/// Erasure by identity, never by label: a homonymous token of another
/// user keeps its rows.
pub async fn delete_by_user(pool: &SqlitePool, user_id: i64) -> Result<u64, sqlx::Error> {
    delete_where(pool, "user_id = ?1", user_id).await
}

/// Deletes the verdicts then the rows of `DELETE_CHUNK` matching
/// resolutions per transaction, yielding between chunks, until fewer than
/// a chunk remain; verdicts go explicitly, never through the cascade.
async fn delete_where(pool: &SqlitePool, predicate: &str, arg: i64) -> Result<u64, sqlx::Error> {
    let ids = format!(
        "SELECT id FROM policy_resolutions WHERE {predicate} ORDER BY id LIMIT {DELETE_CHUNK}"
    );
    let verdicts = format!("DELETE FROM policy_verdicts WHERE resolution_id IN ({ids})");
    let rows = format!("DELETE FROM policy_resolutions WHERE id IN ({ids})");
    let mut total = 0;
    loop {
        let mut tx = pool.begin().await?;
        sqlx::query(&verdicts).bind(arg).execute(&mut *tx).await?;
        let deleted = sqlx::query(&rows)
            .bind(arg)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        total += deleted;
        if (deleted as i64) < DELETE_CHUNK {
            return Ok(total);
        }
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
