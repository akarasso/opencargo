//! `AuditStore` over SQLite.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::error::StoreError;
use crate::ports::audit::{AuditEntry, AuditStore, NewAuditEntry};

const COLUMNS: &str = "id, user_id, username, action, target, repository, ip, user_agent, \
     details_json, created_at";

#[derive(sqlx::FromRow)]
struct EntryRow {
    id: i64,
    user_id: Option<i64>,
    username: Option<String>,
    action: String,
    target: Option<String>,
    repository: Option<String>,
    ip: Option<String>,
    user_agent: Option<String>,
    details_json: Option<String>,
    created_at: String,
}

fn entry_of(row: EntryRow) -> Result<AuditEntry, StoreError> {
    let created_at =
        read_ts(&row.action, "created_at", &row.created_at).map_err(corrupt_row)?;
    Ok(AuditEntry {
        id: row.id,
        user_id: row.user_id,
        username: row.username,
        action: row.action,
        target: row.target,
        repository: row.repository,
        ip: row.ip,
        user_agent: row.user_agent,
        details_json: row.details_json,
        created_at,
    })
}

fn decode(rows: Vec<EntryRow>) -> Result<Vec<AuditEntry>, StoreError> {
    rows.into_iter().map(entry_of).collect()
}

pub struct SqliteAuditStore {
    pool: SqlitePool,
}

impl SqliteAuditStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuditStore for SqliteAuditStore {
    async fn append(
        &self,
        entry: &NewAuditEntry<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log
                 (user_id, username, action, target, repository, ip, user_agent,
                  details_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(entry.user_id)
        .bind(entry.username)
        .bind(entry.action)
        .bind(entry.target)
        .bind(entry.repository)
        .bind(entry.ip)
        .bind(entry.user_agent)
        .bind(entry.details_json)
        .bind(bind_ts(now))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn recent(&self, page: i64, size: i64) -> Result<Vec<AuditEntry>, StoreError> {
        // saturating_*: a huge page must page past the end, not overflow.
        let offset = page.saturating_sub(1).max(0).saturating_mul(size.max(0));
        let rows: Vec<EntryRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM audit_log ORDER BY created_at DESC LIMIT ?1 OFFSET ?2"
        ))
        .bind(size)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        decode(rows)
    }

    async fn of_target(
        &self,
        action: &str,
        target: &str,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let rows: Vec<EntryRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM audit_log WHERE action = ?1 AND target = ?2
             ORDER BY created_at DESC"
        ))
        .bind(action)
        .bind(target)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        decode(rows)
    }
}
