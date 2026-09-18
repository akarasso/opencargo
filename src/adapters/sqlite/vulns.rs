//! `VulnStore` over SQLite.
//!
//! The stored document is read tolerantly on the way out: a row written
//! before the current `ScanResult` shape still happened, and the columns
//! beside it already say how bad it was.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::domain::ScanResult;
use crate::error::StoreError;
use crate::ports::vulns::{VulnScan, VulnStore};

#[derive(sqlx::FromRow)]
struct ScanRow {
    version_id: i64,
    scanned_at: String,
    total_deps: i64,
    vulnerable_deps: i64,
    scan_results_json: Option<String>,
    status: String,
}

fn scan_of(row: ScanRow) -> Result<VulnScan, StoreError> {
    let subject = row.version_id.to_string();
    let scanned_at = read_ts(&subject, "scanned_at", &row.scanned_at).map_err(corrupt_row)?;
    Ok(VulnScan {
        scanned_at,
        total_deps: row.total_deps,
        vulnerable_deps: row.vulnerable_deps,
        status: row.status,
        details: row
            .scan_results_json
            .as_deref()
            .and_then(|stored| serde_json::from_str::<serde_json::Value>(stored).ok())
            .and_then(|mut doc| doc.get_mut("details").map(serde_json::Value::take)),
    })
}

pub struct SqliteVulnStore {
    pool: SqlitePool,
}

impl SqliteVulnStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl VulnStore for SqliteVulnStore {
    async fn latest(&self, version: i64) -> Result<Option<VulnScan>, StoreError> {
        let row: Option<ScanRow> = sqlx::query_as(
            "SELECT version_id, scanned_at, total_deps, vulnerable_deps, scan_results_json, status
             FROM vulnerability_scans WHERE version_id = ?1 ORDER BY scanned_at DESC LIMIT 1",
        )
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(scan_of).transpose()
    }

    async fn record(
        &self,
        version: i64,
        result: &ScanResult,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let document =
            serde_json::to_string(result).map_err(|e| StoreError::Other(Box::new(e)))?;
        sqlx::query(
            "INSERT INTO vulnerability_scans
                 (version_id, scanned_at, total_deps, vulnerable_deps, scan_results_json, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(version)
        .bind(bind_ts(now))
        .bind(result.total_deps as i64)
        .bind(result.vulnerable_deps as i64)
        .bind(document)
        .bind(&result.status)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn forget(&self, version: i64) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM vulnerability_scans WHERE version_id = ?1")
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}
