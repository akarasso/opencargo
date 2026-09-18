//! `PackageStore` over SQLite.
//!
//! Two of the three coarse methods share `write_release`: a publish and a
//! promotion differ in where the bytes came from, not in what they land — a
//! package upsert, a version row and the tags that point at it. The third
//! takes the same rows away.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::{bind_ts, immediate, store_error};
use crate::db::{DistTagRow, PackageRow, VersionRow};
use crate::domain::{DistTag, Package, Version};
use crate::error::StoreError;
use crate::ports::packages::{
    NameMatch, NewRelease, PackageStore, Promotion, PromotionAudit, Release, StalePrerelease,
};

const PACKAGE_COLUMNS: &str =
    "id, repository_id, name, description, readme, license, created_at, updated_at";
const VERSION_COLUMNS: &str = "id, package_id, version, metadata_json, checksum_sha1, \
     checksum_sha256, integrity, size, tarball_path, published_at, yanked";

/// The version columns a release carries, in the order `write_release` binds
/// them.
struct VersionValues<'a> {
    version: &'a str,
    metadata_json: &'a str,
    checksum_sha1: Option<&'a str>,
    checksum_sha256: Option<&'a str>,
    integrity: Option<&'a str>,
    size: i64,
    tarball_path: &'a str,
}

/// A package row to find or create, and the version landing on it.
struct ReleaseSpec<'a> {
    repository: i64,
    package: &'a str,
    match_name: NameMatch,
    description: Option<&'a str>,
    readme: Option<&'a str>,
    values: VersionValues<'a>,
    dist_tags: &'a [String],
    now: DateTime<Utc>,
}

impl<'a> From<&'a NewRelease<'a>> for ReleaseSpec<'a> {
    fn from(release: &'a NewRelease<'a>) -> Self {
        Self {
            repository: release.repository,
            package: release.package,
            match_name: release.match_name,
            description: release.description,
            readme: release.readme,
            values: VersionValues {
                version: release.version,
                metadata_json: release.metadata_json,
                checksum_sha1: release.checksum_sha1,
                checksum_sha256: release.checksum_sha256,
                integrity: release.integrity,
                size: release.size,
                tarball_path: release.tarball_path,
            },
            dist_tags: release.dist_tags,
            now: release.now,
        }
    }
}

impl<'a> From<&'a Promotion<'a>> for ReleaseSpec<'a> {
    fn from(promotion: &'a Promotion<'a>) -> Self {
        Self {
            repository: promotion.target_repository,
            package: promotion.package,
            match_name: NameMatch::Exact,
            description: promotion.description,
            readme: None,
            values: VersionValues {
                version: &promotion.source.version,
                metadata_json: promotion.metadata_json,
                checksum_sha1: promotion.source.checksum_sha1.as_deref(),
                checksum_sha256: promotion.source.checksum_sha256.as_deref(),
                integrity: promotion.source.integrity.as_deref(),
                size: promotion.source.size,
                tarball_path: promotion.tarball_path,
            },
            dist_tags: promotion.dist_tags,
            now: promotion.now,
        }
    }
}

/// The tables whose rows hang off a version. Not one of those foreign keys
/// declares `ON DELETE CASCADE` (`001_initial.sql:41,43,49`), so the version
/// row cannot go until they have.
const VERSION_DEPENDENTS: [&str; 3] = ["dist_tags", "downloads", "download_counts"];

/// A stale pre-release as the join reads it.
#[derive(sqlx::FromRow)]
struct StaleRow {
    id: i64,
    package: String,
    version: String,
    tarball_path: String,
}

impl From<StaleRow> for StalePrerelease {
    fn from(row: StaleRow) -> Self {
        Self {
            id: row.id,
            package: row.package,
            version: row.version,
            tarball_path: row.tarball_path,
        }
    }
}

/// The name predicate, as SQL. `?1` is the repository, `?2` the name.
fn predicate(how: NameMatch) -> &'static str {
    match how {
        NameMatch::Exact => "repository_id = ?1 AND name = ?2",
        NameMatch::Insensitive => "repository_id = ?1 AND name = ?2 COLLATE NOCASE",
    }
}

fn package_of(row: PackageRow) -> Result<Package, StoreError> {
    Package::try_from(row).map_err(|err| StoreError::Other(Box::new(err)))
}

fn version_of(row: VersionRow) -> Result<Version, StoreError> {
    Version::try_from(row).map_err(|err| StoreError::Other(Box::new(err)))
}

/// The package row a release lands on, created when it is not there yet.
async fn upsert_package(
    tx: &mut Transaction<'static, Sqlite>,
    spec: &ReleaseSpec<'_>,
) -> Result<PackageRow, sqlx::Error> {
    let found: Option<PackageRow> = sqlx::query_as(&format!(
        "SELECT {PACKAGE_COLUMNS} FROM packages WHERE {} ORDER BY id LIMIT 1",
        predicate(spec.match_name)
    ))
    .bind(spec.repository)
    .bind(spec.package)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(row) = found {
        return Ok(row);
    }
    sqlx::query_as(&format!(
        "INSERT INTO packages (repository_id, name, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4) RETURNING {PACKAGE_COLUMNS}"
    ))
    .bind(spec.repository)
    .bind(spec.package)
    .bind(spec.description)
    .bind(bind_ts(spec.now))
    .fetch_one(&mut **tx)
    .await
}

/// Package, version and dist-tags: what both coarse methods write.
async fn write_release(
    tx: &mut Transaction<'static, Sqlite>,
    spec: &ReleaseSpec<'_>,
) -> Result<(PackageRow, VersionRow), sqlx::Error> {
    let package = upsert_package(tx, spec).await?;
    if let Some(readme) = spec.readme {
        sqlx::query("UPDATE packages SET readme = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(readme)
            .bind(bind_ts(spec.now))
            .bind(package.id)
            .execute(&mut **tx)
            .await?;
    }

    let values = &spec.values;
    let version: VersionRow = sqlx::query_as(&format!(
        "INSERT INTO versions
             (package_id, version, metadata_json, checksum_sha1, checksum_sha256,
              integrity, size, tarball_path, published_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING {VERSION_COLUMNS}"
    ))
    .bind(package.id)
    .bind(values.version)
    .bind(values.metadata_json)
    .bind(values.checksum_sha1)
    .bind(values.checksum_sha256)
    .bind(values.integrity)
    .bind(values.size)
    .bind(values.tarball_path)
    .bind(bind_ts(spec.now))
    .fetch_one(&mut **tx)
    .await?;

    for tag in spec.dist_tags {
        tag_version(tx, package.id, tag, version.id).await?;
    }
    Ok((package, version))
}

async fn tag_version(
    tx: &mut Transaction<'static, Sqlite>,
    package: i64,
    tag: &str,
    version: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(package_id, tag) DO UPDATE SET version_id = excluded.version_id",
    )
    .bind(package)
    .bind(tag)
    .bind(version)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The promoted version and the record of who promoted it, together.
async fn promote(
    tx: &mut Transaction<'static, Sqlite>,
    spec: &ReleaseSpec<'_>,
    audit: &PromotionAudit<'_>,
) -> Result<VersionRow, sqlx::Error> {
    let (_, version) = write_release(tx, spec).await?;
    write_audit(tx, audit, spec.now).await?;
    Ok(version)
}

async fn write_audit(
    tx: &mut Transaction<'static, Sqlite>,
    audit: &PromotionAudit<'_>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log
             (user_id, username, action, target, repository, details_json, created_at)
         VALUES (?1, ?2, 'package.promote', ?3, ?4, ?5, ?6)",
    )
    .bind(audit.user_id)
    .bind(audit.username)
    .bind(audit.target)
    .bind(audit.repository)
    .bind(audit.details_json)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The dependents then the version itself, inside the caller's transaction.
async fn purge_version(
    tx: &mut Transaction<'static, Sqlite>,
    version: i64,
) -> Result<Result<(), StoreError>, sqlx::Error> {
    for table in VERSION_DEPENDENTS {
        sqlx::query(&format!("DELETE FROM {table} WHERE version_id = ?1"))
            .bind(version)
            .execute(&mut **tx)
            .await?;
    }
    let done = sqlx::query("DELETE FROM versions WHERE id = ?1")
        .bind(version)
        .execute(&mut **tx)
        .await?;
    if done.rows_affected() == 0 {
        return Ok(Err(StoreError::NotFound));
    }
    Ok(Ok(()))
}

pub struct SqlitePackageStore {
    pool: SqlitePool,
}

impl SqlitePackageStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PackageStore for SqlitePackageStore {
    async fn package(
        &self,
        repository: i64,
        name: &str,
        how: NameMatch,
    ) -> Result<Option<Package>, StoreError> {
        let row: Option<PackageRow> = sqlx::query_as(&format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages WHERE {} ORDER BY id LIMIT 1",
            predicate(how)
        ))
        .bind(repository)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(package_of).transpose()
    }

    async fn versions(&self, package: i64) -> Result<Vec<Version>, StoreError> {
        let rows: Vec<VersionRow> = sqlx::query_as(&format!(
            "SELECT {VERSION_COLUMNS} FROM versions WHERE package_id = ?1 ORDER BY id"
        ))
        .bind(package)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter().map(version_of).collect()
    }

    async fn version(
        &self,
        package: i64,
        version: &str,
    ) -> Result<Option<Version>, StoreError> {
        let row: Option<VersionRow> = sqlx::query_as(&format!(
            "SELECT {VERSION_COLUMNS} FROM versions WHERE package_id = ?1 AND version = ?2"
        ))
        .bind(package)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(version_of).transpose()
    }

    async fn dist_tags(&self, package: i64) -> Result<Vec<DistTag>, StoreError> {
        let rows: Vec<DistTagRow> = sqlx::query_as(
            "SELECT id, package_id, tag, version_id FROM dist_tags WHERE package_id = ?1",
        )
        .bind(package)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(DistTag::from).collect())
    }

    async fn publish_version(&self, release: &NewRelease<'_>) -> Result<Release, StoreError> {
        let spec = ReleaseSpec::from(release);
        let (package, version) = immediate(&self.pool, |mut tx| {
            let spec = &spec;
            Box::pin(async move {
                let landed = write_release(&mut tx, spec).await.map(Ok);
                (tx, landed)
            })
        })
        .await?;
        Ok(Release {
            package: package_of(package)?,
            version: version_of(version)?,
        })
    }

    async fn promote_metadata(&self, promotion: &Promotion<'_>) -> Result<Version, StoreError> {
        let spec = ReleaseSpec::from(promotion);
        let version = immediate(&self.pool, |mut tx| {
            let spec = &spec;
            Box::pin(async move {
                let landed = promote(&mut tx, spec, &promotion.audit).await.map(Ok);
                (tx, landed)
            })
        })
        .await?;
        version_of(version)
    }

    async fn set_dist_tag(
        &self,
        package: i64,
        tag: &str,
        version: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO dist_tags (package_id, tag, version_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(package_id, tag) DO UPDATE SET version_id = excluded.version_id",
        )
        .bind(package)
        .bind(tag)
        .bind(version)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn clear_dist_tag(&self, package: i64, tag: &str) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM dist_tags WHERE package_id = ?1 AND tag = ?2")
            .bind(package)
            .bind(tag)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn set_readme(
        &self,
        package: i64,
        readme: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let done = sqlx::query("UPDATE packages SET readme = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(readme)
            .bind(bind_ts(now))
            .bind(package)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn set_metadata(&self, version: i64, metadata_json: &str) -> Result<(), StoreError> {
        let done = sqlx::query("UPDATE versions SET metadata_json = ?1 WHERE id = ?2")
            .bind(metadata_json)
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn set_yanked(&self, version: i64, yanked: bool) -> Result<(), StoreError> {
        let done = sqlx::query("UPDATE versions SET yanked = ?1 WHERE id = ?2")
            .bind(i64::from(yanked))
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn stale_prereleases(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<StalePrerelease>, StoreError> {
        let rows: Vec<StaleRow> = sqlx::query_as(
            "SELECT v.id, p.name AS package, v.version, v.tarball_path
             FROM versions v
             JOIN packages p ON p.id = v.package_id
             JOIN repositories r ON r.id = p.repository_id
             WHERE r.format IN ('npm', 'cargo')
               AND v.version LIKE '%-%'
               AND v.published_at < ?1
             ORDER BY v.id",
        )
        .bind(bind_ts(now - older_than))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(StalePrerelease::from).collect())
    }

    async fn delete_version(&self, version: i64) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let gone = purge_version(&mut tx, version).await;
                (tx, gone)
            })
        })
        .await
    }

    async fn record_download(&self, version: i64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO download_counts (version_id, count) VALUES (?1, 1)
             ON CONFLICT(version_id) DO UPDATE SET count = count + 1",
        )
        .bind(version)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }
}
