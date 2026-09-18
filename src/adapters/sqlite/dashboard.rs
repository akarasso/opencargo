//! `DashboardRead` over SQLite: the twenty queries the web UI's panels used
//! to build by hand, each now on this side of the port.
//!
//! Two things moved here with them. The visibility fragment the handlers
//! spliced into their SQL is a bound [`Reach`], so no caller writes a
//! predicate any more; and the per-row `SELECT … LIMIT 1` each panel ran once
//! per package is one grouped query, because a page of twenty packages used
//! to cost forty-one round trips.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{corrupt_row, public_packages, read_ts, store_error};
use crate::adapters::sqlite::rows::PackageRow;
use crate::domain::Package;
use crate::error::StoreError;
use crate::ports::dashboard::{
    DashboardRead, PackageDetail, PackageFilter, PackagePage, PackageSummary, Reach,
    RecentVersion, TaggedVersion, Totals, VersionSummary,
};

const PACKAGE_COLUMNS: &str =
    "p.id, p.repository_id, p.name, p.description, p.readme, p.license, p.created_at, p.updated_at";

pub struct SqliteDashboardRead {
    pool: SqlitePool,
}

/// The visibility predicate, joined on with the keyword its query needs:
/// `WHERE` when it is the only condition, `AND` when it follows one. An
/// unrestricted reach contributes nothing, which is what the admin arm of the
/// old `visibility_sql` was.
fn only_visible(reach: Reach, keyword: &str, alias: &str) -> String {
    match reach {
        Reach::Everything => String::new(),
        Reach::PublicOnly => format!(" {keyword} {}", public_packages(alias)),
    }
}

/// The list panel's two filter boxes and its reach, as predicates over `p`
/// (and over `r`, which [`from_packages`] joins in for the same condition).
fn filtered<'a>(sql: &mut QueryBuilder<'a, Sqlite>, filter: &PackageFilter<'a>) {
    sql.push(" WHERE 1 = 1");
    if let Some(repository) = filter.repository {
        sql.push(" AND r.name = ").push_bind(repository);
    }
    if let Some(name) = filter.name_contains {
        sql.push(" AND p.name LIKE ").push_bind(format!("%{name}%"));
    }
    if filter.reach == Reach::PublicOnly {
        sql.push(" AND ").push(public_packages("p."));
    }
}

/// The repository join exists only for the name filter: without it the panel
/// reads `packages` alone, as it always has.
fn from_packages<'a>(sql: &mut QueryBuilder<'a, Sqlite>, filter: &PackageFilter<'a>) {
    sql.push(" FROM packages p");
    if filter.repository.is_some() {
        sql.push(" JOIN repositories r ON r.id = p.repository_id");
    }
}

/// One `IN (…)` list of package ids, bound rather than spliced.
fn within<'a>(sql: &mut QueryBuilder<'a, Sqlite>, packages: &[i64]) {
    let mut ids = sql.separated(", ");
    for package in packages {
        ids.push_bind(*package);
    }
}

impl SqliteDashboardRead {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn count(&self, sql: &str) -> Result<i64, StoreError> {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)
    }

    /// How often each of those packages was fetched, absent meaning never.
    async fn downloads_for(&self, packages: &[i64]) -> Result<HashMap<i64, i64>, StoreError> {
        if packages.is_empty() {
            return Ok(HashMap::new());
        }
        let mut sql = QueryBuilder::new(
            "SELECT v.package_id, COALESCE(SUM(dc.count), 0)
             FROM download_counts dc
             JOIN versions v ON v.id = dc.version_id
             WHERE v.package_id IN (",
        );
        within(&mut sql, packages);
        sql.push(") GROUP BY v.package_id");
        let rows: Vec<(i64, i64)> = sql
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(rows.into_iter().collect())
    }

    async fn versions_of(&self, package: i64) -> Result<Vec<VersionSummary>, StoreError> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT version, size, published_at FROM versions
             WHERE package_id = ?1 ORDER BY id DESC",
        )
        .bind(package)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter()
            .map(|(version, size, published_at)| {
                let published_at = read_ts(&version, "published_at", &published_at)?;
                Ok(VersionSummary {
                    version,
                    size,
                    published_at,
                })
            })
            .collect::<Result<_, _>>()
            .map_err(corrupt_row)
    }

    async fn tags_of(&self, package: i64) -> Result<Vec<TaggedVersion>, StoreError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT dt.tag, v.version FROM dist_tags dt
             JOIN versions v ON v.id = dt.version_id
             WHERE dt.package_id = ?1",
        )
        .bind(package)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(tag, version)| TaggedVersion { tag, version })
            .collect())
    }
}

#[async_trait]
impl DashboardRead for SqliteDashboardRead {
    async fn totals(&self, reach: Reach) -> Result<Totals, StoreError> {
        let packages = self
            .count(&format!(
                "SELECT COUNT(*) FROM packages{}",
                only_visible(reach, "WHERE", "")
            ))
            .await?;
        let versions = self
            .count(&format!(
                "SELECT COUNT(*) FROM versions v
                 JOIN packages p ON p.id = v.package_id{}",
                only_visible(reach, "WHERE", "p.")
            ))
            .await?;
        let downloads = self
            .count(&format!(
                "SELECT COALESCE(SUM(dc.count), 0) FROM download_counts dc
                 JOIN versions v ON v.id = dc.version_id
                 JOIN packages p ON p.id = v.package_id{}",
                only_visible(reach, "WHERE", "p.")
            ))
            .await?;
        Ok(Totals {
            packages,
            versions,
            downloads,
        })
    }

    async fn repository_count(&self, reach: Reach) -> Result<i64, StoreError> {
        self.count(match reach {
            Reach::Everything => "SELECT COUNT(*) FROM repositories",
            Reach::PublicOnly => "SELECT COUNT(*) FROM repositories WHERE visibility = 'public'",
        })
        .await
    }

    async fn recent_versions(
        &self,
        reach: Reach,
        limit: i64,
    ) -> Result<Vec<RecentVersion>, StoreError> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(&format!(
            "SELECT p.name, v.version, v.published_at FROM versions v
             JOIN packages p ON p.id = v.package_id{}
             ORDER BY v.published_at DESC LIMIT ?1",
            only_visible(reach, "WHERE", "p.")
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter()
            .map(|(package, version, published_at)| {
                let published_at = read_ts(&package, "published_at", &published_at)?;
                Ok(RecentVersion {
                    package,
                    version,
                    published_at,
                })
            })
            .collect::<Result<_, _>>()
            .map_err(corrupt_row)
    }

    async fn packages(&self, filter: &PackageFilter<'_>) -> Result<PackagePage, StoreError> {
        let mut sql = QueryBuilder::new("SELECT p.id, p.name, p.description, p.updated_at");
        from_packages(&mut sql, filter);
        filtered(&mut sql, filter);
        sql.push(" ORDER BY p.updated_at DESC LIMIT ")
            .push_bind(filter.limit)
            .push(" OFFSET ")
            .push_bind(filter.offset);
        let rows: Vec<(i64, String, Option<String>, String)> = sql
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;

        let mut total = QueryBuilder::new("SELECT COUNT(*)");
        from_packages(&mut total, filter);
        filtered(&mut total, filter);
        let total: i64 = total
            .build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;

        let ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
        let latest = self.latest_versions(&ids).await?;
        let downloads = self.downloads_for(&ids).await?;
        let packages = rows
            .into_iter()
            .map(|(id, name, description, updated_at)| {
                let updated_at = read_ts(&name, "updated_at", &updated_at)?;
                Ok(PackageSummary {
                    latest_version: latest.get(&id).cloned(),
                    downloads: downloads.get(&id).copied().unwrap_or(0),
                    name,
                    description,
                    updated_at,
                })
            })
            .collect::<Result<_, _>>()
            .map_err(corrupt_row)?;
        Ok(PackagePage { packages, total })
    }

    async fn package_detail(
        &self,
        name: &str,
        reach: Reach,
    ) -> Result<Option<PackageDetail>, StoreError> {
        let row: Option<PackageRow> = sqlx::query_as(&format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages p WHERE p.name = ?1{} LIMIT 1",
            only_visible(reach, "AND", "p.")
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let package = Package::try_from(row).map_err(corrupt_row)?;

        let total_downloads = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM downloads d
             JOIN versions v ON v.id = d.version_id
             WHERE v.package_id = ?1",
        )
        .bind(package.id)
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;

        Ok(Some(PackageDetail {
            versions: self.versions_of(package.id).await?,
            dist_tags: self.tags_of(package.id).await?,
            total_downloads,
            package,
        }))
    }

    async fn latest_versions(
        &self,
        packages: &[i64],
    ) -> Result<HashMap<i64, String>, StoreError> {
        if packages.is_empty() {
            return Ok(HashMap::new());
        }
        // The newest row per package, which is what a per-package
        // `ORDER BY id DESC LIMIT 1` was, in one statement.
        let mut sql = QueryBuilder::new(
            "SELECT package_id, version FROM versions WHERE id IN
             (SELECT MAX(id) FROM versions WHERE package_id IN (",
        );
        within(&mut sql, packages);
        sql.push(") GROUP BY package_id)");
        let rows: Vec<(i64, String)> = sql
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(rows.into_iter().collect())
    }
}
