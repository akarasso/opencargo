//! `MavenFileStore` over SQLite (024).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, immediate, read_ts, store_error, Tx};
use crate::error::StoreError;
use crate::ports::maven::{
    Changed, ClientMetadata, Counter, Declaration, Digests, MavenFileStore, PendingUnit,
    StoredFile, SumAlgorithm, Unit, UnitChange, UnitKey, UnitView, Unversioned,
};

#[derive(sqlx::FromRow)]
struct UnitRow {
    id: i64,
    version: String,
    build: String,
    revision: i64,
    depositor: String,
    contested: i64,
    refused: i64,
    visible_at: Option<String>,
    created_at: String,
}

#[derive(sqlx::FromRow)]
struct FileRow {
    unit_id: i64,
    filename: String,
    physical_key: String,
    size: i64,
    sha1: String,
    md5: String,
    sha256: String,
    sha512: String,
    depositor: String,
    created_at: String,
}

const UNIT_COLUMNS: &str = "u.id, v.version, u.build, u.revision, u.depositor, u.contested, \
     u.refused, u.visible_at, u.created_at";
const FILE_COLUMNS: &str = "f.unit_id, f.filename, f.physical_key, f.size, f.sha1, f.md5, \
     f.sha256, f.sha512, f.depositor, f.created_at";

fn ts(column: &'static str, stored: &str) -> Result<DateTime<Utc>, StoreError> {
    read_ts("maven", column, stored).map_err(corrupt_row)
}

fn file_of(row: FileRow) -> Result<StoredFile, StoreError> {
    Ok(StoredFile {
        created_at: ts("created_at", &row.created_at)?,
        filename: row.filename,
        physical_key: row.physical_key,
        size: row.size,
        digests: Digests {
            sha1: row.sha1,
            md5: row.md5,
            sha256: row.sha256,
            sha512: row.sha512,
        },
        depositor: row.depositor,
    })
}

fn visible_at(row: &UnitRow) -> Result<Option<DateTime<Utc>>, StoreError> {
    row.visible_at
        .as_deref()
        .map(|at| ts("visible_at", at))
        .transpose()
}

async fn unit_row(tx: &mut Tx, key: &UnitKey<'_>) -> Result<Option<UnitRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {UNIT_COLUMNS} FROM maven_units u JOIN maven_values v ON v.id = u.value_id
         WHERE v.repository_id = ?1 AND v.ga = ?2 AND v.version = ?3 AND u.build = ?4"
    ))
    .bind(key.repository)
    .bind(key.ga)
    .bind(key.version)
    .bind(key.build)
    .fetch_optional(&mut **tx)
    .await
}

async fn value_id(tx: &mut Tx, key: &UnitKey<'_>) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "INSERT INTO maven_values (repository_id, ga, version) VALUES (?1, ?2, ?3)
         ON CONFLICT(repository_id, ga, version) DO NOTHING",
    )
    .bind(key.repository)
    .bind(key.ga)
    .bind(key.version)
    .execute(&mut **tx)
    .await?;
    sqlx::query_scalar(
        "SELECT id FROM maven_values WHERE repository_id = ?1 AND ga = ?2 AND version = ?3",
    )
    .bind(key.repository)
    .bind(key.ga)
    .bind(key.version)
    .fetch_one(&mut **tx)
    .await
}

/// The unit the change applies to, at its next revision; `None` when the
/// revision it was decided on is gone.
async fn advance(tx: &mut Tx, change: &UnitChange<'_>) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let now = bind_ts(change.now);
    match change.revision {
        None => {
            let value = value_id(tx, &change.key).await?;
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM maven_units WHERE value_id = ?1 AND build = ?2)",
            )
            .bind(value)
            .bind(change.key.build)
            .fetch_one(&mut **tx)
            .await?;
            if taken {
                return Ok(None);
            }
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO maven_units (value_id, build, depositor, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, 1) RETURNING id",
            )
            .bind(value)
            .bind(change.key.build)
            .bind(change.depositor)
            .bind(&now)
            .fetch_one(&mut **tx)
            .await?;
            Ok(Some((id, 1)))
        }
        Some(revision) => {
            let Some(row) = unit_row(tx, &change.key).await? else {
                return Ok(None);
            };
            let done = sqlx::query(
                "UPDATE maven_units SET revision = revision + 1 WHERE id = ?1 AND revision = ?2",
            )
            .bind(row.id)
            .bind(revision)
            .execute(&mut **tx)
            .await?;
            Ok((done.rows_affected() == 1).then_some((row.id, revision + 1)))
        }
    }
}

async fn bump(tx: &mut Tx, repository: i64, scopes: &[String], now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    for scope in scopes {
        sqlx::query(
            "INSERT INTO maven_counters (repository_id, scope, counter, updated_at)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(repository_id, scope)
             DO UPDATE SET counter = counter + 1, updated_at = excluded.updated_at",
        )
        .bind(repository)
        .bind(scope)
        .bind(bind_ts(now))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The file inserted or replaced; the replaced key, when it is another one.
async fn put_file(
    tx: &mut Tx,
    unit: i64,
    change: &UnitChange<'_>,
) -> Result<Vec<String>, sqlx::Error> {
    let Some(file) = &change.file else {
        return Ok(Vec::new());
    };
    let old: Option<String> = sqlx::query_scalar(
        "SELECT physical_key FROM maven_files WHERE unit_id = ?1 AND filename = ?2",
    )
    .bind(unit)
    .bind(file.filename)
    .fetch_optional(&mut **tx)
    .await?;
    if old.is_some() {
        sqlx::query("DELETE FROM maven_declarations WHERE unit_id = ?1 AND filename = ?2")
            .bind(unit)
            .bind(file.filename)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO maven_files
             (unit_id, filename, physical_key, size, sha1, md5, sha256, sha512, depositor, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(unit_id, filename) DO UPDATE SET
             physical_key = excluded.physical_key, size = excluded.size, sha1 = excluded.sha1,
             md5 = excluded.md5, sha256 = excluded.sha256, sha512 = excluded.sha512,
             depositor = excluded.depositor, created_at = excluded.created_at",
    )
    .bind(unit)
    .bind(file.filename)
    .bind(file.physical_key)
    .bind(file.size)
    .bind(&file.digests.sha1)
    .bind(&file.digests.md5)
    .bind(&file.digests.sha256)
    .bind(&file.digests.sha512)
    .bind(file.depositor)
    .bind(bind_ts(change.now))
    .execute(&mut **tx)
    .await?;
    let released: Vec<String> = old.into_iter().filter(|k| k != file.physical_key).collect();
    super::reclaim::enqueue_keys(tx, &released, change.now).await?;
    Ok(released)
}

async fn apply(tx: &mut Tx, change: &UnitChange<'_>) -> Result<Result<Changed, StoreError>, sqlx::Error> {
    if let Err(revoked) = super::reclaim::spend_pins(tx, change.pins).await? {
        return Ok(Err(StoreError::Superseded(revoked)));
    }
    let Some((unit, revision)) = advance(tx, change).await? else {
        return Ok(Err(StoreError::Conflict));
    };
    let released = put_file(tx, unit, change).await?;
    for d in change.declarations {
        sqlx::query(
            "INSERT INTO maven_declarations (unit_id, filename, algorithm, value, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(unit_id, filename, algorithm) DO UPDATE SET value = excluded.value",
        )
        .bind(unit)
        .bind(&d.filename)
        .bind(d.algorithm.as_str())
        .bind(&d.value)
        .bind(bind_ts(change.now))
        .execute(&mut **tx)
        .await?;
    }
    if change.contest {
        sqlx::query("UPDATE maven_units SET contested = 1 WHERE id = ?1")
            .bind(unit)
            .execute(&mut **tx)
            .await?;
    }
    if change.reveal {
        sqlx::query("UPDATE maven_units SET visible_at = ?2 WHERE id = ?1 AND visible_at IS NULL")
            .bind(unit)
            .bind(bind_ts(change.now))
            .execute(&mut **tx)
            .await?;
    }
    bump(tx, change.key.repository, change.scopes, change.now).await?;
    Ok(Ok(Changed { revision, released }))
}

async fn refuse_unit(
    tx: &mut Tx,
    key: &UnitKey<'_>,
    revision: i64,
    scopes: &[String],
    now: DateTime<Utc>,
) -> Result<Result<Changed, StoreError>, sqlx::Error> {
    let Some(row) = unit_row(tx, key).await? else {
        return Ok(Err(StoreError::NotFound));
    };
    let done = sqlx::query(
        "UPDATE maven_units SET revision = revision + 1, refused = 1 WHERE id = ?1 AND revision = ?2",
    )
    .bind(row.id)
    .bind(revision)
    .execute(&mut **tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(Err(StoreError::Conflict));
    }
    let released: Vec<String> =
        sqlx::query_scalar("SELECT physical_key FROM maven_files WHERE unit_id = ?1 ORDER BY filename")
            .bind(row.id)
            .fetch_all(&mut **tx)
            .await?;
    for table in ["maven_files", "maven_declarations"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE unit_id = ?1"))
            .bind(row.id)
            .execute(&mut **tx)
            .await?;
    }
    super::reclaim::enqueue_keys(tx, &released, now).await?;
    bump(tx, key.repository, scopes, now).await?;
    Ok(Ok(Changed {
        revision: revision + 1,
        released,
    }))
}

fn plugins_json(plugins: &[(String, String, String)]) -> String {
    serde_json::Value::Array(
        plugins
            .iter()
            .map(|(prefix, artifact, name)| serde_json::json!([prefix, artifact, name]))
            .collect(),
    )
    .to_string()
}

fn plugins_of(stored: Option<&str>) -> Vec<(String, String, String)> {
    let Some(parsed) = stored.and_then(|s| serde_json::from_str::<Vec<[String; 3]>>(s).ok()) else {
        return Vec::new();
    };
    parsed
        .into_iter()
        .map(|[prefix, artifact, name]| (prefix, artifact, name))
        .collect()
}

pub struct SqliteMavenFileStore {
    pool: SqlitePool,
}

impl SqliteMavenFileStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn files_of(&self, units: &[i64]) -> Result<Vec<FileRow>, StoreError> {
        if units.is_empty() {
            return Ok(Vec::new());
        }
        let ids = units.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        sqlx::query_as(&format!(
            "SELECT {FILE_COLUMNS} FROM maven_files f WHERE f.unit_id IN ({ids}) ORDER BY f.filename"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)
    }
}

#[async_trait]
impl MavenFileStore for SqliteMavenFileStore {
    async fn unit(&self, key: &UnitKey<'_>) -> Result<Option<Unit>, StoreError> {
        let row: Option<UnitRow> = sqlx::query_as(&format!(
            "SELECT {UNIT_COLUMNS} FROM maven_units u JOIN maven_values v ON v.id = u.value_id
             WHERE v.repository_id = ?1 AND v.ga = ?2 AND v.version = ?3 AND u.build = ?4"
        ))
        .bind(key.repository)
        .bind(key.ga)
        .bind(key.version)
        .bind(key.build)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let files = self
            .files_of(&[row.id])
            .await?
            .into_iter()
            .map(file_of)
            .collect::<Result<Vec<_>, _>>()?;
        let declared: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT filename, algorithm, value FROM maven_declarations
             WHERE unit_id = ?1 ORDER BY filename, algorithm",
        )
        .bind(row.id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        let mut declarations = Vec::with_capacity(declared.len());
        for (filename, algorithm, value) in declared {
            let algorithm = SumAlgorithm::parse(&algorithm).ok_or_else(|| {
                corrupt_row(crate::domain::DomainError::CorruptColumn {
                    repo: "maven".to_string(),
                    column: "algorithm",
                    value: algorithm.clone(),
                })
            })?;
            declarations.push(Declaration {
                filename,
                algorithm,
                value,
            });
        }
        Ok(Some(Unit {
            visible_at: visible_at(&row)?,
            created_at: ts("created_at", &row.created_at)?,
            version: row.version,
            build: row.build,
            revision: row.revision,
            depositor: row.depositor,
            contested: row.contested == 1,
            refused: row.refused == 1,
            files,
            declarations,
        }))
    }

    async fn change(&self, change: &UnitChange<'_>) -> Result<Changed, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = apply(&mut tx, change).await;
                (tx, done)
            })
        })
        .await
    }

    async fn refuse(
        &self,
        key: &UnitKey<'_>,
        revision: i64,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<Changed, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = refuse_unit(&mut tx, key, revision, scopes, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn artifact(&self, repository: i64, ga: &str) -> Result<Vec<UnitView>, StoreError> {
        let rows: Vec<UnitRow> = sqlx::query_as(&format!(
            "SELECT {UNIT_COLUMNS} FROM maven_units u JOIN maven_values v ON v.id = u.value_id
             WHERE v.repository_id = ?1 AND v.ga = ?2 ORDER BY u.id"
        ))
        .bind(repository)
        .bind(ga)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let mut files = self.files_of(&ids).await?;
        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let (mine, rest): (Vec<FileRow>, Vec<FileRow>) =
                files.into_iter().partition(|f| f.unit_id == row.id);
            files = rest;
            views.push(UnitView {
                visible_at: visible_at(&row)?,
                version: row.version,
                build: row.build,
                refused: row.refused == 1,
                files: mine.into_iter().map(file_of).collect::<Result<_, _>>()?,
            });
        }
        Ok(views)
    }

    async fn counter(&self, repository: i64, scope: &str) -> Result<Counter, StoreError> {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT counter, updated_at FROM maven_counters WHERE repository_id = ?1 AND scope = ?2",
        )
        .bind(repository)
        .bind(scope)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        match row {
            Some((value, at)) => Ok(Counter {
                value,
                updated_at: Some(ts("updated_at", &at)?),
            }),
            None => Ok(Counter::default()),
        }
    }

    async fn record_client_metadata(
        &self,
        metadata: &ClientMetadata,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    sqlx::query(
                        "INSERT INTO maven_client_metadata
                             (repository_id, dir, sha1, md5, sha256, sha512, release, latest,
                              plugins, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                         ON CONFLICT(repository_id, dir) DO UPDATE SET
                             sha1 = excluded.sha1, md5 = excluded.md5, sha256 = excluded.sha256,
                             sha512 = excluded.sha512, release = excluded.release,
                             latest = excluded.latest, plugins = excluded.plugins,
                             updated_at = excluded.updated_at",
                    )
                    .bind(metadata.repository)
                    .bind(&metadata.dir)
                    .bind(&metadata.digests.sha1)
                    .bind(&metadata.digests.md5)
                    .bind(&metadata.digests.sha256)
                    .bind(&metadata.digests.sha512)
                    .bind(&metadata.release)
                    .bind(&metadata.latest)
                    .bind(plugins_json(&metadata.plugins))
                    .bind(bind_ts(now))
                    .execute(&mut *tx)
                    .await?;
                    bump(&mut tx, metadata.repository, scopes, now).await?;
                    Ok(Ok(()))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }

    async fn client_metadata(
        &self,
        repository: i64,
        dir: &str,
    ) -> Result<Option<ClientMetadata>, StoreError> {
        type Row = (String, String, String, String, Option<String>, Option<String>, Option<String>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT sha1, md5, sha256, sha512, release, latest, plugins
             FROM maven_client_metadata WHERE repository_id = ?1 AND dir = ?2",
        )
        .bind(repository)
        .bind(dir)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(|(sha1, md5, sha256, sha512, release, latest, plugins)| ClientMetadata {
            repository,
            dir: dir.to_string(),
            digests: Digests {
                sha1,
                md5,
                sha256,
                sha512,
            },
            release,
            latest,
            plugins: plugins_of(plugins.as_deref()),
        }))
    }

    async fn pending(
        &self,
        before: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<PendingUnit>, StoreError> {
        let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
            "SELECT v.repository_id, v.ga, v.version, u.build, u.created_at
             FROM maven_units u JOIN maven_values v ON v.id = u.value_id
             WHERE u.visible_at IS NULL AND u.refused = 0 AND u.created_at < ?1
             ORDER BY u.created_at, u.id LIMIT ?2",
        )
        .bind(bind_ts(before))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter()
            .map(|(repository, ga, version, build, at)| {
                Ok(PendingUnit {
                    repository,
                    ga,
                    version,
                    build,
                    created_at: ts("created_at", &at)?,
                })
            })
            .collect()
    }

    async fn unversioned(&self, limit: u32) -> Result<Vec<Unversioned>, StoreError> {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT v.repository_id, v.ga, v.version FROM maven_values v
             WHERE v.versioned = 0 AND EXISTS (
                 SELECT 1 FROM maven_units u
                 WHERE u.value_id = v.id AND u.visible_at IS NOT NULL AND u.refused = 0)
             ORDER BY v.id LIMIT ?1",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(repository, ga, version)| Unversioned {
                repository,
                ga,
                version,
            })
            .collect())
    }

    async fn mark_versioned(
        &self,
        repository: i64,
        ga: &str,
        version: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE maven_values SET versioned = 1
             WHERE repository_id = ?1 AND ga = ?2 AND version = ?3",
        )
        .bind(repository)
        .bind(ga)
        .bind(version)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }
}
