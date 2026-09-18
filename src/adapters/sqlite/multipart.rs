//! `MultipartLedger` over SQLite: four single-statement writes and a count.
//!
//! None of them opens a transaction, because none of them writes twice — a
//! coarse method exists only where two writes must be atomic, and bookkeeping
//! for one upload is one row.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, store_error};
use crate::error::StoreError;
use crate::ports::multipart::{Abandoned, MultipartLedger};

pub struct SqliteMultipartLedger {
    pool: SqlitePool,
}

impl SqliteMultipartLedger {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MultipartLedger for SqliteMultipartLedger {
    async fn opened(
        &self,
        upload_id: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let at = bind_ts(now);
        sqlx::query(
            "INSERT INTO storage_multipart (upload_id, object_key, started_at, touched_at)
             VALUES (?1, ?2, ?3, ?3)",
        )
        .bind(upload_id)
        .bind(key)
        .bind(&at)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn touched(&self, upload_id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("UPDATE storage_multipart SET touched_at = ?2 WHERE upload_id = ?1")
            .bind(upload_id)
            .bind(bind_ts(now))
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn closed(&self, upload_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM storage_multipart WHERE upload_id = ?1")
            .bind(upload_id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn idle_since(
        &self,
        age: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<Abandoned>, StoreError> {
        let cutoff = TimeDelta::from_std(age)
            .ok()
            .and_then(|age| now.checked_sub_signed(age))
            .ok_or_else(|| StoreError::Other("multipart sweep age is out of range".into()))?;
        sqlx::query_as(
            "SELECT upload_id, object_key FROM storage_multipart
             WHERE touched_at < ?1 ORDER BY touched_at",
        )
        .bind(bind_ts(cutoff))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn in_flight(&self) -> Result<u64, StoreError> {
        let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_multipart")
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(open.unsigned_abs())
    }
}

#[cfg(test)]
#[path = "multipart_tests.rs"]
mod tests;
