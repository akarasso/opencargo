//! `RoutingRuleStore` over SQLite. Lists cross the column boundary as JSON,
//! and every write bumps the snapshot version in the same transaction as the
//! row it changed: a version that could lag its rules would let a memo keyed
//! on it serve an answer the new rule forbids.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, immediate, read_ts, store_error, Tx};
use crate::domain::{DomainError, Effect, Format};
use crate::error::StoreError;
use crate::ports::routing::{NewRule, RoutingRuleStore, StoredRule};

const COLUMNS: &str =
    "name, format, patterns, except_idents, effect, targets, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct RuleRow {
    name: String,
    format: String,
    patterns: String,
    except_idents: String,
    effect: String,
    targets: String,
    created_at: String,
    updated_at: String,
}

fn list(rule: &str, column: &'static str, raw: &str) -> Result<Vec<String>, DomainError> {
    serde_json::from_str(raw).map_err(|_| DomainError::CorruptColumn {
        repo: rule.to_string(),
        column,
        value: raw.to_string(),
    })
}

impl TryFrom<RuleRow> for StoredRule {
    type Error = DomainError;

    fn try_from(row: RuleRow) -> Result<Self, DomainError> {
        let corrupt = |column: &'static str, value: &str| DomainError::CorruptColumn {
            repo: row.name.clone(),
            column,
            value: value.to_string(),
        };
        let format: Format = row
            .format
            .parse()
            .map_err(|_| corrupt("format", &row.format))?;
        let effect = match row.effect.as_str() {
            "deny" => Effect::Deny,
            "allow_hosted" => Effect::AnyHosted,
            "allow_members" => Effect::Members(list(&row.name, "targets", &row.targets)?),
            other => return Err(corrupt("effect", other)),
        };
        Ok(StoredRule {
            patterns: list(&row.name, "patterns", &row.patterns)?,
            except: list(&row.name, "except_idents", &row.except_idents)?,
            created_at: read_ts(&row.name, "created_at", &row.created_at)?,
            updated_at: read_ts(&row.name, "updated_at", &row.updated_at)?,
            name: row.name,
            format,
            effect,
        })
    }
}

fn decode(row: RuleRow) -> Result<StoredRule, StoreError> {
    StoredRule::try_from(row).map_err(corrupt_row)
}

fn targets(effect: &Effect) -> String {
    match effect {
        Effect::Members(incarnations) => json(incarnations),
        _ => "[]".to_string(),
    }
}

fn json(values: &[String]) -> String {
    serde_json::to_string(values).expect("a list of strings always serializes")
}

pub struct SqliteRoutingRuleStore {
    pool: SqlitePool,
}

impl SqliteRoutingRuleStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// The one statement every write ends with: the snapshot the readers key on
/// moves exactly when a rule does.
async fn bump(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE routing_snapshot_version SET version = version + 1 WHERE id = 1")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn write(
    tx: &mut Tx,
    rule: &NewRule<'_>,
    now: DateTime<Utc>,
    creating: bool,
) -> Result<(), sqlx::Error> {
    let statement = if creating {
        "INSERT INTO routing_rules (name, format, patterns, except_idents, effect, targets, \
         created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)"
    } else {
        "UPDATE routing_rules SET format = ?2, patterns = ?3, except_idents = ?4, effect = ?5, \
         targets = ?6, updated_at = ?7 WHERE name = ?1"
    };
    sqlx::query(statement)
        .bind(rule.name)
        .bind(rule.format.as_str())
        .bind(json(rule.patterns))
        .bind(json(rule.except))
        .bind(rule.effect.as_str())
        .bind(targets(rule.effect))
        .bind(bind_ts(now))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn fetch(tx: &mut Tx, name: &str) -> Result<Option<RuleRow>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM routing_rules WHERE name = ?1"))
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
}

/// Write the rule and move the version, then read the row back: the answer is
/// what the next reader will see, not what the caller asked for.
async fn upsert(
    tx: &mut Tx,
    rule: &NewRule<'_>,
    now: DateTime<Utc>,
    creating: bool,
) -> Result<Result<RuleRow, StoreError>, sqlx::Error> {
    match (fetch(tx, rule.name).await?, creating) {
        (Some(_), true) => return Ok(Err(StoreError::Conflict)),
        (None, false) => return Ok(Err(StoreError::NotFound)),
        _ => {}
    }
    write(tx, rule, now, creating).await?;
    bump(tx).await?;
    Ok(fetch(tx, rule.name).await?.ok_or(StoreError::NotFound))
}

async fn remove(tx: &mut Tx, name: &str) -> Result<Result<(), StoreError>, sqlx::Error> {
    let done = sqlx::query("DELETE FROM routing_rules WHERE name = ?1")
        .bind(name)
        .execute(&mut **tx)
        .await?;
    if done.rows_affected() == 0 {
        return Ok(Err(StoreError::NotFound));
    }
    bump(tx).await?;
    Ok(Ok(()))
}

/// The emptiness test and the inserts are one transaction: two nodes booting
/// together seed once between them, and the loser writes nothing.
async fn seed(
    tx: &mut Tx,
    rules: &[NewRule<'_>],
    now: DateTime<Utc>,
) -> Result<Result<usize, StoreError>, sqlx::Error> {
    let present: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routing_rules")
        .fetch_one(&mut **tx)
        .await?;
    if present > 0 || rules.is_empty() {
        return Ok(Ok(0));
    }
    for rule in rules {
        write(tx, rule, now, true).await?;
    }
    bump(tx).await?;
    Ok(Ok(rules.len()))
}

#[async_trait]
impl RoutingRuleStore for SqliteRoutingRuleStore {
    async fn all(&self) -> Result<Vec<StoredRule>, StoreError> {
        let rows: Vec<RuleRow> =
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM routing_rules ORDER BY name"))
                .fetch_all(&self.pool)
                .await
                .map_err(store_error)?;
        rows.into_iter().map(decode).collect()
    }

    async fn by_name(&self, name: &str) -> Result<Option<StoredRule>, StoreError> {
        let row: Option<RuleRow> =
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM routing_rules WHERE name = ?1"))
                .bind(name)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        row.map(decode).transpose()
    }

    async fn create(
        &self,
        rule: &NewRule<'_>,
        now: DateTime<Utc>,
    ) -> Result<StoredRule, StoreError> {
        let row = immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let written = upsert(&mut tx, rule, now, true).await;
                (tx, written)
            })
        })
        .await?;
        decode(row)
    }

    async fn update(
        &self,
        rule: &NewRule<'_>,
        now: DateTime<Utc>,
    ) -> Result<StoredRule, StoreError> {
        let row = immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let written = upsert(&mut tx, rule, now, false).await;
                (tx, written)
            })
        })
        .await?;
        decode(row)
    }

    async fn delete(&self, name: &str) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let removed = remove(&mut tx, name).await;
                (tx, removed)
            })
        })
        .await
    }

    async fn version(&self) -> Result<u64, StoreError> {
        let version: Option<i64> =
            sqlx::query_scalar("SELECT version FROM routing_snapshot_version WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        Ok(version.unwrap_or(0).max(0) as u64)
    }

    async fn ensure_seeded(
        &self,
        rules: &[NewRule<'_>],
        now: DateTime<Utc>,
    ) -> Result<usize, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let inserted = seed(&mut tx, rules, now).await;
                (tx, inserted)
            })
        })
        .await
    }
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
