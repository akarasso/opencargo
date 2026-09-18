//! `ProxyCacheStore` over SQLite: one statement per method, every instant
//! bound as `bind_ts(now)`.
//!
//! No statement here spells `datetime('now')`. Freshness and retention both
//! compare a stored column against a clock, and SQLite compares those columns
//! as text — so the comparison is only correct while both sides carry the same
//! rendering. Binding the caller's instant keeps that rendering in one place
//! and leaves the predicate itself dialect-free.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, parse_ts, store_error};
use crate::domain::{CacheEntry, CacheEntryId, NewEntry, RepoId};
use crate::error::StoreError;
use crate::ports::proxy_cache::ProxyCacheStore;

const COLUMNS: &str = "id, repository_id, kind, cache_key, status, storage_path, content_type, \
                       etag, digest, size, fetched_at, expires_at, last_used_at";

/// `?1` is the reading clock: an entry with no expiry is immutable.
const FRESH: &str = "(expires_at IS NULL OR expires_at > ?1) AS fresh";

#[derive(sqlx::FromRow)]
struct CacheRow {
    id: i64,
    repository_id: i64,
    kind: String,
    cache_key: String,
    status: i64,
    storage_path: Option<String>,
    content_type: Option<String>,
    etag: Option<String>,
    digest: Option<String>,
    size: i64,
    fetched_at: String,
    expires_at: Option<String>,
    last_used_at: String,
    fresh: bool,
}

impl TryFrom<CacheRow> for CacheEntry {
    type Error = StoreError;

    fn try_from(row: CacheRow) -> Result<Self, StoreError> {
        Ok(CacheEntry {
            id: row.id,
            repository_id: row.repository_id,
            kind: row.kind,
            cache_key: row.cache_key,
            status: row.status,
            storage_path: row.storage_path,
            content_type: row.content_type,
            etag: row.etag,
            digest: row.digest,
            size: row.size,
            fetched_at: at("fetched_at", &row.fetched_at)?,
            expires_at: row.expires_at.as_deref().map(|v| at("expires_at", v)).transpose()?,
            last_used_at: at("last_used_at", &row.last_used_at)?,
            fresh: row.fresh,
        })
    }
}

fn at(column: &str, stored: &str) -> Result<DateTime<Utc>, StoreError> {
    parse_ts(stored)
        .ok_or_else(|| StoreError::Other(format!("proxy cache {column} is not a timestamp: {stored}").into()))
}

pub struct SqliteProxyCacheStore {
    pool: SqlitePool,
}

impl SqliteProxyCacheStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProxyCacheStore for SqliteProxyCacheStore {
    async fn entry(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheEntry>, StoreError> {
        let row: Option<CacheRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS}, {FRESH} FROM proxy_cache_entries
             WHERE repository_id = ?2 AND kind = ?3 AND cache_key = ?4"
        ))
        .bind(bind_ts(now))
        .bind(repo)
        .bind(kind)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(CacheEntry::try_from).transpose()
    }

    async fn upsert(&self, entry: &NewEntry<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO proxy_cache_entries
                 (repository_id, kind, cache_key, status, storage_path, content_type, etag,
                  digest, size, fetched_at, expires_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?10)
             ON CONFLICT(repository_id, kind, cache_key) DO UPDATE SET
                 status = excluded.status, storage_path = excluded.storage_path,
                 content_type = excluded.content_type, etag = excluded.etag,
                 digest = excluded.digest, size = excluded.size,
                 fetched_at = excluded.fetched_at, expires_at = excluded.expires_at,
                 last_used_at = excluded.last_used_at",
        )
        .bind(entry.repository_id)
        .bind(entry.kind)
        .bind(entry.cache_key)
        .bind(entry.status)
        .bind(entry.storage_path)
        .bind(entry.content_type)
        .bind(entry.etag)
        .bind(entry.digest)
        .bind(entry.size)
        .bind(bind_ts(now))
        .bind(CacheEntry::expiry(entry.ttl(), now).map(bind_ts))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn touch(
        &self,
        id: CacheEntryId,
        ttl: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE proxy_cache_entries
             SET last_used_at = ?1, expires_at = COALESCE(?2, expires_at)
             WHERE id = ?3",
        )
        .bind(bind_ts(now))
        .bind(CacheEntry::expiry(ttl, now).map(bind_ts))
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn delete_for_repo(&self, repo: RepoId) -> Result<u64, StoreError> {
        let gone = sqlx::query("DELETE FROM proxy_cache_entries WHERE repository_id = ?1")
            .bind(repo)
            .execute(&self.pool)
            .await
            .map_err(store_error)?
            .rows_affected();
        // Pre-`013` installs still carry rows in the table that preceded this one.
        sqlx::query("DELETE FROM proxy_cache_meta WHERE repository_id = ?1")
            .bind(repo)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(gone)
    }

    async fn evictable(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CacheEntry>, StoreError> {
        let rows: Vec<CacheRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS}, {FRESH} FROM proxy_cache_entries
             WHERE (status <> 200 AND expires_at <= ?1) OR last_used_at < ?2
             ORDER BY id LIMIT ?3"
        ))
        .bind(bind_ts(now))
        .bind(bind_ts(now - idle))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter().map(CacheEntry::try_from).collect()
    }

    async fn delete(&self, id: CacheEntryId) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM proxy_cache_entries WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}
