//! `PypiFileStore` over SQLite (019).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, immediate, read_ts, store_error, Tx};
use crate::error::StoreError;
use crate::ports::pypi::{NewPypiFile, Published, PypiFile, PypiFileStore};

/// This port's contribution to the reclaim predicate: every artifact and
/// `.metadata` key a file row references.
pub(crate) const REFERENCED: &str = "
    SELECT storage_key AS k, 0 AS p FROM pypi_files
    UNION ALL
    SELECT metadata_key, 0 FROM pypi_files WHERE metadata_key IS NOT NULL";

const FILE_COLUMNS: &str = "f.id, f.repository_id, f.package_id, f.version_id, p.name AS project, \
     v.version, f.filename, f.packagetype, f.sha256, f.size, f.storage_key, f.metadata_key, \
     f.metadata_sha256, f.requires_python, f.yanked, f.yanked_reason, f.uploaded_at";

const FILE_JOIN: &str =
    "pypi_files f JOIN packages p ON p.id = f.package_id JOIN versions v ON v.id = f.version_id";

const VERSION_DEPENDENTS: [&str; 3] = ["dist_tags", "downloads", "download_counts"];

#[derive(sqlx::FromRow)]
struct FileRow {
    id: i64,
    repository_id: i64,
    package_id: i64,
    version_id: i64,
    project: String,
    version: String,
    filename: String,
    packagetype: String,
    sha256: String,
    size: i64,
    storage_key: String,
    metadata_key: Option<String>,
    metadata_sha256: Option<String>,
    requires_python: Option<String>,
    yanked: i64,
    yanked_reason: Option<String>,
    uploaded_at: String,
}

fn file_of(row: FileRow) -> Result<PypiFile, StoreError> {
    let uploaded_at = read_ts(&row.filename, "uploaded_at", &row.uploaded_at).map_err(corrupt_row)?;
    Ok(PypiFile {
        id: row.id,
        repository: row.repository_id,
        package_id: row.package_id,
        version_id: row.version_id,
        project: row.project,
        version: row.version,
        filename: row.filename,
        packagetype: row.packagetype,
        sha256: row.sha256,
        size: row.size,
        key: row.storage_key,
        metadata_key: row.metadata_key,
        metadata_sha256: row.metadata_sha256,
        requires_python: row.requires_python,
        yanked: row.yanked != 0,
        yanked_reason: row.yanked_reason,
        uploaded_at,
    })
}

async fn package_id(tx: &mut Tx, file: &NewPypiFile<'_>) -> Result<i64, sqlx::Error> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT id FROM packages WHERE repository_id = ?1 AND name = ?2")
            .bind(file.repository)
            .bind(file.project)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(id) = found {
        return Ok(id);
    }
    sqlx::query_scalar(
        "INSERT INTO packages (repository_id, name, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
    )
    .bind(file.repository)
    .bind(file.project)
    .bind(file.summary)
    .bind(bind_ts(file.now))
    .fetch_one(&mut **tx)
    .await
}

async fn version_id(
    tx: &mut Tx,
    package: i64,
    file: &NewPypiFile<'_>,
) -> Result<(i64, bool), sqlx::Error> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT id FROM versions WHERE package_id = ?1 AND version = ?2")
            .bind(package)
            .bind(file.version)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(id) = found {
        return Ok((id, false));
    }
    let id = sqlx::query_scalar(
        "INSERT INTO versions (package_id, version, metadata_json, checksum_sha256, size,
                               tarball_path, published_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
    )
    .bind(package)
    .bind(file.version)
    .bind(file.metadata_json)
    .bind(file.sha256)
    .bind(file.size)
    .bind(&file.pins[0].physical_key)
    .bind(bind_ts(file.now))
    .fetch_one(&mut **tx)
    .await?;
    Ok((id, true))
}

async fn write_file(tx: &mut Tx, file: &NewPypiFile<'_>) -> Result<(i64, bool), sqlx::Error> {
    let package = package_id(tx, file).await?;
    let (version, created) = version_id(tx, package, file).await?;
    let id = sqlx::query_scalar(
        "INSERT INTO pypi_files (repository_id, package_id, version_id, filename, packagetype,
                                 sha256, size, storage_key, metadata_key, metadata_sha256,
                                 requires_python, uploaded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) RETURNING id",
    )
    .bind(file.repository)
    .bind(package)
    .bind(version)
    .bind(file.filename)
    .bind(file.packagetype)
    .bind(file.sha256)
    .bind(file.size)
    .bind(&file.pins[0].physical_key)
    .bind(file.pins.get(1).map(|p| p.physical_key.as_str()))
    .bind(file.metadata_sha256)
    .bind(file.requires_python)
    .bind(bind_ts(file.now))
    .fetch_one(&mut **tx)
    .await?;
    Ok((id, created))
}

async fn release_versions(
    tx: &mut Tx,
    repository: i64,
    project: &str,
    version: Option<&str>,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT v.id FROM versions v JOIN packages p ON p.id = v.package_id
         WHERE p.repository_id = ?1 AND p.name = ?2 AND (?3 IS NULL OR v.version = ?3)
         ORDER BY v.id",
    )
    .bind(repository)
    .bind(project)
    .bind(version)
    .fetch_all(&mut **tx)
    .await
}

/// The files of `versions`, then the versions, and every key they held
/// enqueued in the same transaction.
async fn purge(
    tx: &mut Tx,
    versions: &[i64],
    now: DateTime<Utc>,
) -> Result<Vec<String>, sqlx::Error> {
    let mut keys = Vec::new();
    for &version in versions {
        let held: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT storage_key, metadata_key FROM pypi_files WHERE version_id = ?1")
                .bind(version)
                .fetch_all(&mut **tx)
                .await?;
        for (key, metadata) in held {
            keys.push(key);
            keys.extend(metadata);
        }
        let tarball: String = sqlx::query_scalar("SELECT tarball_path FROM versions WHERE id = ?1")
            .bind(version)
            .fetch_one(&mut **tx)
            .await?;
        keys.push(tarball);
        sqlx::query("DELETE FROM pypi_files WHERE version_id = ?1")
            .bind(version)
            .execute(&mut **tx)
            .await?;
        for table in VERSION_DEPENDENTS {
            sqlx::query(&format!("DELETE FROM {table} WHERE version_id = ?1"))
                .bind(version)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("DELETE FROM versions WHERE id = ?1")
            .bind(version)
            .execute(&mut **tx)
            .await?;
    }
    keys.sort();
    keys.dedup();
    super::reclaim::enqueue_keys(tx, &keys, now).await?;
    Ok(keys)
}

pub struct SqlitePypiFileStore {
    pool: SqlitePool,
}

impl SqlitePypiFileStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn delete(
        &self,
        repository: i64,
        project: &str,
        version: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let versions = release_versions(&mut tx, repository, project, version).await?;
                    if versions.is_empty() {
                        return Ok(Err(StoreError::NotFound));
                    }
                    purge(&mut tx, &versions, now).await.map(Ok)
                }
                .await;
                (tx, done)
            })
        })
        .await
    }
}

#[async_trait]
impl PypiFileStore for SqlitePypiFileStore {
    async fn publish_file(&self, file: &NewPypiFile<'_>) -> Result<Published, StoreError> {
        if file.pins.is_empty() {
            return Err(StoreError::Other("a file row needs its artifact's pin".into()));
        }
        let (id, version_created) = immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let landed = match super::reclaim::spend_pins(&mut tx, file.pins).await {
                    Ok(Ok(())) => write_file(&mut tx, file).await.map(Ok),
                    Ok(Err(revoked)) => Ok(Err(StoreError::Superseded(revoked))),
                    Err(e) => Err(e),
                };
                (tx, landed)
            })
        })
        .await?;
        let row: FileRow = sqlx::query_as(&format!(
            "SELECT {FILE_COLUMNS} FROM {FILE_JOIN} WHERE f.id = ?1"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(Published {
            version_created,
            file: file_of(row)?,
        })
    }

    async fn file_by_name(
        &self,
        repository: i64,
        filename: &str,
    ) -> Result<Option<PypiFile>, StoreError> {
        let row: Option<FileRow> = sqlx::query_as(&format!(
            "SELECT {FILE_COLUMNS} FROM {FILE_JOIN} WHERE f.repository_id = ?1 AND f.filename = ?2"
        ))
        .bind(repository)
        .bind(filename)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(file_of).transpose()
    }

    async fn project_files(
        &self,
        repository: i64,
        project: &str,
    ) -> Result<Vec<PypiFile>, StoreError> {
        let rows: Vec<FileRow> = sqlx::query_as(&format!(
            "SELECT {FILE_COLUMNS} FROM {FILE_JOIN}
             WHERE f.repository_id = ?1 AND p.name = ?2 ORDER BY f.id"
        ))
        .bind(repository)
        .bind(project)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter().map(file_of).collect()
    }

    async fn list_projects(&self, repository: i64) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT DISTINCT p.name FROM pypi_files f JOIN packages p ON p.id = f.package_id
             WHERE f.repository_id = ?1 ORDER BY p.name",
        )
        .bind(repository)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn set_release_yanked(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        reason: Option<&str>,
        yanked: bool,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let reason = if yanked { reason } else { None };
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let versions =
                        release_versions(&mut tx, repository, project, Some(version)).await?;
                    let Some(&id) = versions.first() else {
                        return Ok(Err(StoreError::NotFound));
                    };
                    sqlx::query("UPDATE versions SET yanked = ?1 WHERE id = ?2")
                        .bind(i64::from(yanked))
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(
                        "UPDATE pypi_files SET yanked = ?1, yanked_reason = ?2 WHERE version_id = ?3",
                    )
                    .bind(i64::from(yanked))
                    .bind(reason)
                    .bind(id)
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

    async fn delete_release(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        self.delete(repository, project, Some(version), now).await
    }

    async fn delete_project_files(
        &self,
        repository: i64,
        project: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        self.delete(repository, project, None, now).await
    }
}
