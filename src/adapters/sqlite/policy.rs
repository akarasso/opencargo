//! `PolicyStore` over SQLite.
//!
//! Two things here are not obvious. The report's window predicate compares
//! `created_at` against a *bound* value in this adapter's own timestamp
//! format, so it sorts lexicographically against the rows already there; and
//! retention takes the cut-off from the caller instead of asking the database
//! for `datetime('now', '-N days')`, which is the one piece of date
//! arithmetic no second dialect could have copied.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, corrupt_row, immediate, read_ts, store_error};
use crate::error::StoreError;
use crate::ports::policy::{
    IdRange, NewResolution, PolicyStore, ReportFilter, ResolutionRow, RuleTotals, Subject,
    Totals, VerdictRow,
};

/// Rows deleted per transaction: the write lock is held for one chunk, never
/// for a whole day of resolutions and their verdicts.
pub const DELETE_CHUNK: i64 = 5000;

#[derive(sqlx::FromRow)]
struct StoredResolution {
    id: i64,
    created_at: String,
    requested_repo: String,
    member_repo: String,
    format: String,
    name: String,
    version: Option<String>,
    digest: Option<String>,
    date_source: String,
    actor: String,
    actor_kind: String,
    user_id: Option<i64>,
    published_at: Option<String>,
    would_block: bool,
    unknown: bool,
}

fn resolution_of(row: StoredResolution) -> Result<ResolutionRow, StoreError> {
    let subject = row.id.to_string();
    let created_at =
        read_ts(&subject, "created_at", &row.created_at).map_err(corrupt_row)?;
    let published_at = row
        .published_at
        .as_deref()
        .map(|stored| read_ts(&subject, "published_at", stored))
        .transpose()
        .map_err(corrupt_row)?;
    Ok(ResolutionRow {
        id: row.id,
        created_at,
        requested_repo: row.requested_repo,
        member_repo: row.member_repo,
        format: row.format,
        name: row.name,
        version: row.version,
        digest: row.digest,
        date_source: row.date_source,
        actor: row.actor,
        actor_kind: row.actor_kind,
        user_id: row.user_id,
        published_at,
        would_block: row.would_block,
        unknown: row.unknown,
    })
}

#[derive(sqlx::FromRow)]
struct StoredVerdict {
    resolution_id: i64,
    rule: String,
    verdict: String,
    reason: String,
}

impl From<StoredVerdict> for VerdictRow {
    fn from(row: StoredVerdict) -> Self {
        VerdictRow {
            resolution_id: row.resolution_id,
            rule: row.rule,
            verdict: row.verdict,
            reason: row.reason,
        }
    }
}

#[derive(sqlx::FromRow)]
struct TotalsHead {
    resolutions: i64,
    would_block: i64,
    unknown: i64,
}

#[derive(sqlx::FromRow)]
struct RuleCount {
    rule: String,
    verdict: String,
    count: i64,
}

/// `FROM policy_resolutions r [JOIN that rule's verdict v]`, the scan every
/// report query shares.
fn from<'a>(q: &mut QueryBuilder<'a, Sqlite>, f: &ReportFilter<'a>) {
    q.push(" FROM policy_resolutions r");
    if let Some(rule) = f.rule {
        q.push(" JOIN policy_verdicts v ON v.resolution_id = r.id AND v.rule = ");
        q.push_bind(rule);
    }
}

fn filters<'a>(q: &mut QueryBuilder<'a, Sqlite>, f: &ReportFilter<'a>, range: Option<IdRange>) {
    q.push(" WHERE r.created_at >= ");
    q.push_bind(bind_ts(f.since));
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

fn tally(counts: Vec<RuleCount>) -> BTreeMap<String, RuleTotals> {
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
    by_rule
}

/// One resolution and its verdicts inside the batch's transaction.
async fn write_resolution(
    tx: &mut super::Tx,
    row: &NewResolution<'_>,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let would_block = row
        .verdicts
        .iter()
        .any(|v| v.verdict == crate::domain::Verdict::WouldBlock);
    let unknown = !would_block
        && row
            .verdicts
            .iter()
            .any(|v| v.verdict == crate::domain::Verdict::Unknown);
    let id = sqlx::query(
        "INSERT INTO policy_resolutions
             (created_at, requested_repo, member_repo, format, name, version, digest,
              published_at, date_source, actor, actor_kind, user_id, would_block, unknown)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )
    .bind(bind_ts(now))
    .bind(row.requested_repo)
    .bind(row.member_repo)
    .bind(row.format)
    .bind(row.name)
    .bind(row.version)
    .bind(row.digest)
    .bind(
        row.published_at
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
    )
    .bind(row.date_source)
    .bind(row.actor)
    .bind(row.actor_kind)
    .bind(row.user_id)
    .bind(would_block)
    .bind(unknown)
    .execute(&mut **tx)
    .await?
    .last_insert_rowid();
    for v in row.verdicts {
        sqlx::query(
            "INSERT INTO policy_verdicts (resolution_id, rule, verdict, reason)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(id)
        .bind(v.rule)
        .bind(v.verdict.as_str())
        .bind(&v.reason)
        .execute(&mut **tx)
        .await?;
    }
    Ok(id)
}

pub struct SqlitePolicyStore {
    pool: SqlitePool,
}

impl SqlitePolicyStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Deletes the verdicts then the rows of `DELETE_CHUNK` matching
    /// resolutions per transaction, yielding between chunks, until fewer than
    /// a chunk remain; verdicts go explicitly, never through the cascade.
    async fn delete_where<A>(&self, predicate: &str, arg: A) -> Result<u64, StoreError>
    where
        A: for<'q> sqlx::Encode<'q, Sqlite> + sqlx::Type<Sqlite> + Clone + Send,
    {
        let ids = format!(
            "SELECT id FROM policy_resolutions WHERE {predicate} ORDER BY id LIMIT {DELETE_CHUNK}"
        );
        let verdicts = format!("DELETE FROM policy_verdicts WHERE resolution_id IN ({ids})");
        let rows = format!("DELETE FROM policy_resolutions WHERE id IN ({ids})");
        let mut total = 0;
        loop {
            let mut tx = self.pool.begin().await.map_err(store_error)?;
            sqlx::query(&verdicts)
                .bind(arg.clone())
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            let deleted = sqlx::query(&rows)
                .bind(arg.clone())
                .execute(&mut *tx)
                .await
                .map_err(store_error)?
                .rows_affected();
            tx.commit().await.map_err(store_error)?;
            total += deleted;
            if (deleted as i64) < DELETE_CHUNK {
                return Ok(total);
            }
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait]
impl PolicyStore for SqlitePolicyStore {
    async fn insert_batch(
        &self,
        rows: &[NewResolution<'_>],
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let mut ids = Vec::with_capacity(rows.len());
                for row in rows {
                    match write_resolution(&mut tx, row, now).await {
                        Ok(id) => ids.push(id),
                        Err(e) => return (tx, Err(e)),
                    }
                }
                (tx, Ok(Ok(ids)))
            })
        })
        .await
    }

    async fn max_id(&self) -> Result<i64, StoreError> {
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM policy_resolutions")
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)
    }

    async fn totals(
        &self,
        filter: &ReportFilter<'_>,
        range: IdRange,
    ) -> Result<Totals, StoreError> {
        let mut q = QueryBuilder::new("SELECT COUNT(*) AS resolutions, ");
        q.push(match filter.rule {
            Some(_) => "COALESCE(SUM(v.verdict = 'would_block'), 0) AS would_block, COALESCE(SUM(v.verdict = 'unknown'), 0) AS unknown",
            None => "COALESCE(SUM(r.would_block), 0) AS would_block, COALESCE(SUM(r.unknown), 0) AS unknown",
        });
        from(&mut q, filter);
        filters(&mut q, filter, Some(range));
        let head: TotalsHead = q
            .build_query_as()
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;

        let verdicts = if filter.rule.is_some() { "v" } else { "pv" };
        let mut q = QueryBuilder::new(format!(
            "SELECT {verdicts}.rule, {verdicts}.verdict, COUNT(*) AS count"
        ));
        from(&mut q, filter);
        if filter.rule.is_none() {
            q.push(" JOIN policy_verdicts pv ON pv.resolution_id = r.id");
        }
        filters(&mut q, filter, Some(range));
        q.push(" GROUP BY 1, 2");
        let counts: Vec<RuleCount> = q
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;

        Ok(Totals {
            resolutions: head.resolutions as u64,
            would_block: head.would_block as u64,
            unknown: head.unknown as u64,
            by_rule: tally(counts),
        })
    }

    async fn resolutions(
        &self,
        filter: &ReportFilter<'_>,
        page: i64,
        size: i64,
    ) -> Result<Vec<ResolutionRow>, StoreError> {
        let mut q = QueryBuilder::new(
            "SELECT r.id, r.created_at, r.requested_repo, r.member_repo, r.format, r.name, \
             r.version, r.digest, r.date_source, r.actor, r.actor_kind, r.user_id, \
             r.published_at, ",
        );
        q.push(flags(filter));
        from(&mut q, filter);
        filters(&mut q, filter, None);
        q.push(" ORDER BY r.id DESC LIMIT ");
        q.push_bind(size);
        q.push(" OFFSET ");
        q.push_bind(page.saturating_sub(1).max(0).saturating_mul(size.max(0)));
        let rows: Vec<StoredResolution> = q
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        rows.into_iter().map(resolution_of).collect()
    }

    async fn verdicts_for(
        &self,
        ids: &[i64],
        rule: Option<&str>,
    ) -> Result<Vec<VerdictRow>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut q = QueryBuilder::new(
            "SELECT resolution_id, rule, verdict, reason FROM policy_verdicts \
             WHERE resolution_id IN (",
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
        let rows: Vec<StoredVerdict> = q
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(rows.into_iter().map(VerdictRow::from).collect())
    }

    async fn delete_older_than(
        &self,
        days: u64,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        // A retention so long it leaves the representable range keeps every
        // row, which is what "older than for ever" has to mean.
        let cutoff = i64::try_from(days)
            .ok()
            .and_then(chrono::Duration::try_days)
            .and_then(|span| now.checked_sub_signed(span))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        self.delete_where("created_at < ?1", bind_ts(cutoff)).await
    }

    async fn erase_user(&self, user_id: i64) -> Result<u64, StoreError> {
        self.delete_where("user_id = ?1", user_id).await
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
