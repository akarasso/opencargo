//! `ReclaimStore` and `ReferencedKeys` over SQLite (025). One predicate,
//! [`REFERENCED`], answers both "what does a committed row reference" for the
//! read model and "is this key referenced" inside a claim's transaction.
//!
//! The enqueue helpers are also what `delete_version` and `retire` call in
//! their own transactions.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::stream;
use sqlx::SqlitePool;

use super::{bind_ts, immediate, store_error, Tx};
use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::reclaim::{
    Backlog, Candidate, Claim, ClaimToken, Pinned, PinToken, ReclaimStore, Renewal,
};
use crate::ports::referenced::{Referenced, ReferencedKeys, ReferencedStream};

/// Every key a committed row references, `p = 1` for a key that protects
/// everything under it.
pub(crate) const REFERENCED: &str = "
    SELECT tarball_path AS k, 0 AS p FROM versions
    UNION ALL
    SELECT storage_path, 0 FROM proxy_cache_entries WHERE storage_path IS NOT NULL
    UNION ALL
    SELECT storage_key, 0 FROM oci_blobs WHERE storage_key IS NOT NULL
    UNION ALL
    SELECT storage_key, 0 FROM oci_manifests WHERE storage_key IS NOT NULL
    UNION ALL
    SELECT COALESCE(segment_prefix, 'oci/_uploads/' || id), 1 FROM oci_uploads";

/// Every port's contribution, as one union.
fn referenced() -> String {
    format!("{REFERENCED} UNION ALL {}", super::pypi::REFERENCED)
}

/// `?1` is referenced, or (for a prefix) something under it is, or a
/// protecting key covers it.
const REFERENCES_KEY: &str = "
    SELECT EXISTS (
        SELECT 1 FROM (REFERENCED) refs
        WHERE refs.k = ?1
           OR (?2 = 1 AND substr(refs.k, 1, length(?1) + 1) = ?1 || '/')
           OR (refs.p = 1 AND substr(?1, 1, length(refs.k) + 1) = refs.k || '/')
    )";

fn references_key() -> String {
    REFERENCES_KEY.replace("REFERENCED", &referenced())
}

pub(crate) async fn enqueue_keys(
    tx: &mut Tx,
    keys: &[String],
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    for key in keys {
        sqlx::query(
            "INSERT INTO reclaim_candidates (key, prefix, enqueued_at) VALUES (?1, 0, ?2)
             ON CONFLICT(key) DO NOTHING",
        )
        .bind(key)
        .bind(bind_ts(now))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(crate) async fn enqueue_prefix(
    tx: &mut Tx,
    prefix: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO reclaim_candidates (key, prefix, enqueued_at) VALUES (?1, 1, ?2)
         ON CONFLICT(key) DO UPDATE SET prefix = 1",
    )
    .bind(prefix)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Every pin under `prefix`, or taken for it, is revoked.
pub(crate) async fn revoke_under(tx: &mut Tx, prefix: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM reclaim_pins
         WHERE repo_prefix = ?1 OR physical_key = ?1
            OR substr(physical_key, 1, length(?1) + 1) = ?1 || '/'",
    )
    .bind(prefix)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The tokens a commit spends, all or none: `Err` names the physical keys
/// whose pin was revoked or pruned. Called inside the committing method's
/// own transaction.
pub(crate) async fn spend_pins(
    tx: &mut Tx,
    tokens: &[PinToken],
) -> Result<Result<(), Vec<String>>, sqlx::Error> {
    let mut revoked = Vec::new();
    for pin in tokens {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM reclaim_pins WHERE token = ?1 AND physical_key = ?2)",
        )
        .bind(&pin.token)
        .bind(&pin.physical_key)
        .fetch_one(&mut **tx)
        .await?;
        if !live {
            revoked.push(pin.physical_key.clone());
        }
    }
    if !revoked.is_empty() {
        return Ok(Err(revoked));
    }
    for pin in tokens {
        sqlx::query("DELETE FROM reclaim_pins WHERE token = ?1")
            .bind(&pin.token)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Ok(()))
}

async fn is_retired(tx: &mut Tx, repo_prefix: &str) -> Result<bool, sqlx::Error> {
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM storage_prefixes s
             WHERE s.prefix = ?1
               AND s.incarnation NOT IN (SELECT incarnation FROM retired_incarnations)
         )",
    )
    .bind(repo_prefix)
    .fetch_one(&mut **tx)
    .await?;
    Ok(!live)
}

/// A generation of `logical` that a committed row references and no claim
/// ever took.
async fn reusable(tx: &mut Tx, logical: &str) -> Result<Option<String>, sqlx::Error> {
    let stem = layout::physical_key(logical, "");
    let referenced = referenced();
    sqlx::query_scalar(&format!(
        "SELECT refs.k FROM ({referenced}) refs
         WHERE refs.p = 0 AND substr(refs.k, 1, length(?1)) = ?1
           AND instr(substr(refs.k, length(?1) + 1), '/') = 0
           AND refs.k NOT IN (SELECT physical_key FROM reclaim_claimed)
         LIMIT 1"
    ))
    .bind(&stem)
    .fetch_optional(&mut **tx)
    .await
}

async fn pin_all(
    tx: &mut Tx,
    repo_prefix: &str,
    logical_keys: &[String],
    until: DateTime<Utc>,
) -> Result<Result<Pinned, StoreError>, sqlx::Error> {
    if is_retired(tx, repo_prefix).await? {
        return Ok(Ok(Pinned::Retired));
    }
    let mut tokens = Vec::with_capacity(logical_keys.len());
    for logical in logical_keys {
        let physical = match reusable(tx, logical).await? {
            Some(physical) => physical,
            None => layout::physical_key(logical, &uuid::Uuid::new_v4().simple().to_string()),
        };
        let token = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO reclaim_pins (token, physical_key, repo_prefix, until)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&token)
        .bind(&physical)
        .bind(repo_prefix)
        .bind(bind_ts(until))
        .execute(&mut **tx)
        .await?;
        tokens.push(PinToken {
            token,
            logical_key: logical.clone(),
            physical_key: physical,
        });
    }
    Ok(Ok(Pinned::Tokens(tokens)))
}

fn cutoff(grace: Duration, now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX)
}

async fn claim_one(
    tx: &mut Tx,
    key: &str,
    grace: Duration,
    now: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Result<Claim, StoreError>, sqlx::Error> {
    let candidate: Option<(i64, String)> =
        sqlx::query_as("SELECT prefix, enqueued_at FROM reclaim_candidates WHERE key = ?1")
            .bind(key)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((prefix, enqueued_at)) = candidate else {
        return Ok(Ok(Claim::NotDue));
    };
    let retired = prefix == 1 && is_retired_prefix(tx, key).await?;
    if prefix == 1 && !retired && is_live_prefix(tx, key).await? {
        sqlx::query("DELETE FROM reclaim_candidates WHERE key = ?1")
            .bind(key)
            .execute(&mut **tx)
            .await?;
        return Ok(Ok(Claim::Referenced));
    }
    if !retired && enqueued_at > bind_ts(cutoff(grace, now)) {
        return Ok(Ok(Claim::NotDue));
    }
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM reclaim_claims WHERE key = ?1 AND until > ?2)",
    )
    .bind(key)
    .bind(bind_ts(now))
    .fetch_one(&mut **tx)
    .await?;
    if held {
        return Ok(Ok(Claim::NotDue));
    }
    let referenced: bool = sqlx::query_scalar(&references_key())
        .bind(key)
        .bind(prefix)
        .fetch_one(&mut **tx)
        .await?;
    if referenced {
        sqlx::query("DELETE FROM reclaim_candidates WHERE key = ?1")
            .bind(key)
            .execute(&mut **tx)
            .await?;
        return Ok(Ok(Claim::Referenced));
    }
    let protected: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM reclaim_pins
             WHERE until > ?3
               AND (physical_key = ?1
                    OR (?2 = 1 AND substr(physical_key, 1, length(?1) + 1) = ?1 || '/'))
         )",
    )
    .bind(key)
    .bind(prefix)
    .bind(bind_ts(cutoff(grace, now)))
    .fetch_one(&mut **tx)
    .await?;
    if protected && !retired {
        return Ok(Ok(Claim::Pinned));
    }
    if prefix == 1 {
        revoke_under(tx, key).await?;
    } else {
        sqlx::query("DELETE FROM reclaim_pins WHERE physical_key = ?1")
            .bind(key)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "INSERT INTO reclaim_claimed (physical_key, claimed_at) VALUES (?1, ?2)
             ON CONFLICT(physical_key) DO NOTHING",
        )
        .bind(key)
        .bind(bind_ts(now))
        .execute(&mut **tx)
        .await?;
    }
    let token = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO reclaim_claims (key, token, until) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET token = excluded.token, until = excluded.until",
    )
    .bind(key)
    .bind(&token)
    .bind(bind_ts(until))
    .execute(&mut **tx)
    .await?;
    Ok(Ok(Claim::Claimed(ClaimToken(token))))
}

async fn is_live_prefix(tx: &mut Tx, prefix: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM storage_prefixes
             WHERE prefix = ?1
               AND incarnation NOT IN (SELECT incarnation FROM retired_incarnations)
         )",
    )
    .bind(prefix)
    .fetch_one(&mut **tx)
    .await
}

async fn is_retired_prefix(tx: &mut Tx, prefix: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM storage_prefixes s JOIN retired_incarnations r
                 ON r.incarnation = s.incarnation
             WHERE s.prefix = ?1
         )",
    )
    .bind(prefix)
    .fetch_one(&mut **tx)
    .await
}

pub struct SqliteReclaimStore {
    pool: SqlitePool,
}

impl SqliteReclaimStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ReclaimStore for SqliteReclaimStore {
    async fn pin(
        &self,
        repo_prefix: &str,
        logical_keys: &[String],
        until: DateTime<Utc>,
    ) -> Result<Pinned, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let pinned = pin_all(&mut tx, repo_prefix, logical_keys, until).await;
                (tx, pinned)
            })
        })
        .await
    }

    async fn enqueue(&self, physical_keys: &[String], now: DateTime<Utc>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = enqueue_keys(&mut tx, physical_keys, now).await.map(Ok);
                (tx, done)
            })
        })
        .await
    }

    async fn enqueue_prefix(&self, prefix: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = enqueue_prefix(&mut tx, prefix, now).await.map(Ok);
                (tx, done)
            })
        })
        .await
    }

    async fn due(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Candidate>, StoreError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT c.key, c.prefix FROM reclaim_candidates c
             WHERE (c.enqueued_at <= ?1
                    OR (c.prefix = 1 AND c.key IN (
                        SELECT s.prefix FROM storage_prefixes s
                        JOIN retired_incarnations r ON r.incarnation = s.incarnation)))
               AND NOT EXISTS (
                   SELECT 1 FROM reclaim_claims k WHERE k.key = c.key AND k.until > ?2)
             ORDER BY c.enqueued_at, c.key
             LIMIT ?3",
        )
        .bind(bind_ts(cutoff(grace, now)))
        .bind(bind_ts(now))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(key, prefix)| Candidate {
                key,
                prefix: prefix == 1,
            })
            .collect())
    }

    async fn claim(
        &self,
        key: &str,
        grace: Duration,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Claim, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let claimed = claim_one(&mut tx, key, grace, now, until).await;
                (tx, claimed)
            })
        })
        .await
    }

    async fn renew(
        &self,
        token: &ClaimToken,
        _now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Renewal, StoreError> {
        let done = sqlx::query("UPDATE reclaim_claims SET until = ?2 WHERE token = ?1")
            .bind(&token.0)
            .bind(bind_ts(until))
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(if done.rows_affected() == 1 {
            Renewal::Renewed
        } else {
            Renewal::Superseded
        })
    }

    async fn release(&self, token: &ClaimToken) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let key: Option<String> =
                        sqlx::query_scalar("SELECT key FROM reclaim_claims WHERE token = ?1")
                            .bind(&token.0)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if let Some(key) = key {
                        sqlx::query("DELETE FROM reclaim_claims WHERE token = ?1")
                            .bind(&token.0)
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("DELETE FROM reclaim_candidates WHERE key = ?1")
                            .bind(&key)
                            .execute(&mut *tx)
                            .await?;
                    }
                    Ok(Ok(()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn forget_claimed(&self, physical_key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM reclaim_claimed WHERE physical_key = ?1")
            .bind(physical_key)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn forget_retired(&self, prefix: &str) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let incarnation: Option<String> = sqlx::query_scalar(
                        "SELECT incarnation FROM storage_prefixes WHERE prefix = ?1
                         AND incarnation IN (SELECT incarnation FROM retired_incarnations)",
                    )
                    .bind(prefix)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let Some(incarnation) = incarnation else {
                        return Ok(Ok(()));
                    };
                    sqlx::query("DELETE FROM storage_prefixes WHERE prefix = ?1")
                        .bind(prefix)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(
                        "DELETE FROM retired_incarnations WHERE incarnation = ?1
                         AND NOT EXISTS (SELECT 1 FROM storage_prefixes WHERE incarnation = ?1)",
                    )
                    .bind(&incarnation)
                    .execute(&mut *tx)
                    .await?;
                    Ok(Ok(()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn backlog(&self) -> Result<Backlog, StoreError> {
        let (candidates, prefixes): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(prefix), 0) FROM reclaim_candidates",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(Backlog {
            candidates: candidates.unsigned_abs(),
            prefixes: prefixes.unsigned_abs(),
        })
    }

    async fn prune_pins(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError> {
        let done = sqlx::query(
            "DELETE FROM reclaim_pins WHERE token IN (
                 SELECT token FROM reclaim_pins WHERE until <= ?1 ORDER BY until LIMIT ?2)",
        )
        .bind(bind_ts(cutoff(grace, now)))
        .bind(i64::from(limit))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(done.rows_affected())
    }
}

pub struct SqliteReferencedKeys {
    pool: SqlitePool,
}

impl SqliteReferencedKeys {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const PAGE: i64 = 1000;

impl ReferencedKeys for SqliteReferencedKeys {
    fn referenced(&self, grace: Duration, now: DateTime<Utc>) -> ReferencedStream {
        let pool = self.pool.clone();
        let since = bind_ts(cutoff(grace, now));
        let referenced = referenced();
        let query = format!(
            "SELECT k, p FROM ({referenced}
                 UNION ALL SELECT physical_key, 0 FROM reclaim_pins WHERE until > ?1)
             ORDER BY k, p LIMIT ?2 OFFSET ?3"
        );
        Box::pin(stream::try_unfold(
            (0i64, Vec::<Referenced>::new(), false),
            move |(offset, mut page, done)| {
                let pool = pool.clone();
                let since = since.clone();
                let query = query.clone();
                async move {
                    if page.is_empty() && !done {
                        let rows: Vec<(String, i64)> = sqlx::query_as(&query)
                            .bind(&since)
                            .bind(PAGE)
                            .bind(offset)
                            .fetch_all(&pool)
                            .await
                            .map_err(store_error)?;
                        let last = (rows.len() as i64) < PAGE;
                        page = rows
                            .into_iter()
                            .rev()
                            .map(|(key, p)| Referenced { key, prefix: p == 1 })
                            .collect();
                        return match page.pop() {
                            Some(first) => Ok(Some((first, (offset + PAGE, page, last)))),
                            None => Ok(None),
                        };
                    }
                    match page.pop() {
                        Some(next) => Ok(Some((next, (offset, page, done)))),
                        None => Ok(None),
                    }
                }
            },
        ))
    }
}
