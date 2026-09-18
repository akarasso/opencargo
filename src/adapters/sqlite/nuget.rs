//! Port 17 over SQLite: the candidates of one repository read in one
//! transaction, so a publish landing meanwhile is wholly in or wholly out,
//! then the port's own filter and window.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::SqlitePool;

use super::store_error;
use crate::adapters::sqlite::rows::{PackageRow, VersionRow};
use crate::domain::{Package, Version};
use crate::error::StoreError;
use crate::ports::nuget::{FeedPage, FeedQuery, NugetFeedRead};

pub struct SqliteNugetFeed {
    pool: SqlitePool,
}

impl SqliteNugetFeed {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn decode<R, T>(rows: Vec<R>) -> Result<Vec<T>, StoreError>
where
    T: TryFrom<R>,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    rows.into_iter()
        .map(|r| T::try_from(r).map_err(|e| StoreError::Other(Box::new(e))))
        .collect()
}

#[async_trait]
impl NugetFeedRead for SqliteNugetFeed {
    async fn search(&self, query: &FeedQuery<'_>) -> Result<FeedPage, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let packages: Vec<PackageRow> = sqlx::query_as(
            "SELECT id, repository_id, name, description, readme, license, created_at, updated_at
             FROM packages WHERE repository_id = ?1",
        )
        .bind(query.repository)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let versions: Vec<VersionRow> = sqlx::query_as(
            "SELECT v.id, v.package_id, v.version, v.metadata_json, v.checksum_sha1,
                    v.checksum_sha256, v.integrity, v.size, v.tarball_path, v.published_at, v.yanked
             FROM versions v JOIN packages p ON p.id = v.package_id
             WHERE p.repository_id = ?1 AND v.yanked = 0 ORDER BY v.id",
        )
        .bind(query.repository)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let downloads: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT v.package_id, COALESCE(SUM(c.count), 0)
             FROM versions v JOIN packages p ON p.id = v.package_id
             JOIN download_counts c ON c.version_id = v.id
             WHERE p.repository_id = ?1 GROUP BY v.package_id",
        )
        .bind(query.repository)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;

        let packages: Vec<Package> = decode(packages)?;
        let versions: Vec<Version> = decode(versions)?;
        let downloads: HashMap<i64, i64> = downloads.into_iter().collect();
        let mut by_package: HashMap<i64, Vec<Version>> = HashMap::new();
        for v in versions {
            by_package.entry(v.package_id).or_default().push(v);
        }
        let candidates = packages
            .into_iter()
            .map(|p| {
                let vs = by_package.remove(&p.id).unwrap_or_default();
                let d = downloads.get(&p.id).copied().unwrap_or(0);
                (p, vs, d)
            })
            .collect();
        Ok(query.page(candidates))
    }
}
