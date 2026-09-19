//! `RawFileStore` over SQLite (026).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, immediate, read_ts, store_error, Tx};
use crate::error::StoreError;
use crate::ports::raw::{NewRawFile, RawFile, RawFileStore, Stored};

/// This port's contribution to the reclaim predicate.
pub(crate) const REFERENCED: &str = "SELECT physical_key AS k, 0 AS p FROM raw_files";

const COLUMNS: &str =
    "repository_id, path, physical_key, size, sha256, content_type, uploaded_by, uploaded_at";

#[derive(sqlx::FromRow)]
struct FileRow {
    repository_id: i64,
    path: String,
    physical_key: String,
    size: i64,
    sha256: String,
    content_type: Option<String>,
    uploaded_by: String,
    uploaded_at: String,
}

fn file_of(row: FileRow) -> Result<RawFile, StoreError> {
    let uploaded_at = read_ts(&row.path, "uploaded_at", &row.uploaded_at).map_err(corrupt_row)?;
    Ok(RawFile {
        repository: row.repository_id,
        path: row.path,
        physical_key: row.physical_key,
        size: row.size,
        sha256: row.sha256,
        content_type: row.content_type,
        uploaded_by: row.uploaded_by,
        uploaded_at,
    })
}

/// Every `_` and `%` in `prefix` is a literal here, so the escape character
/// the `LIKE` is given has to be declared.
fn like_prefix(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}/%")
}

/// The row as the transaction leaves it, read back before it commits: a
/// read after the commit could answer with another writer's row.
async fn write_file(
    tx: &mut Tx,
    file: &NewRawFile<'_>,
) -> Result<Result<Stored, StoreError>, sqlx::Error> {
    let held: Option<String> =
        sqlx::query_scalar("SELECT physical_key FROM raw_files WHERE repository_id = ?1 AND path = ?2")
            .bind(file.repository)
            .bind(file.path)
            .fetch_optional(&mut **tx)
            .await?;
    let key = &file.pins[0].physical_key;
    sqlx::query(&format!(
        "INSERT INTO raw_files ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(repository_id, path) DO UPDATE SET
             physical_key = excluded.physical_key, size = excluded.size,
             sha256 = excluded.sha256, content_type = excluded.content_type,
             uploaded_by = excluded.uploaded_by, uploaded_at = excluded.uploaded_at"
    ))
    .bind(file.repository)
    .bind(file.path)
    .bind(key)
    .bind(file.size)
    .bind(file.sha256)
    .bind(file.content_type)
    .bind(file.uploaded_by)
    .bind(bind_ts(file.now))
    .execute(&mut **tx)
    .await?;
    let created = held.is_none();
    let released: Vec<String> = held.filter(|old| old != key).into_iter().collect();
    super::reclaim::enqueue_keys(tx, &released, file.now).await?;
    let row: FileRow = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM raw_files WHERE repository_id = ?1 AND path = ?2"
    ))
    .bind(file.repository)
    .bind(file.path)
    .fetch_one(&mut **tx)
    .await?;
    Ok(file_of(row).map(|file| Stored {
        file,
        created,
        released,
    }))
}

pub struct SqliteRawFileStore {
    pool: SqlitePool,
}

impl SqliteRawFileStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RawFileStore for SqliteRawFileStore {
    async fn put_file(&self, file: &NewRawFile<'_>) -> Result<Stored, StoreError> {
        let Some(_pin) = file.pins.first() else {
            return Err(StoreError::Other("a raw file row needs its pin".into()));
        };
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let landed = match super::reclaim::spend_pins(&mut tx, file.pins).await {
                    Ok(Ok(())) => write_file(&mut tx, file).await,
                    Ok(Err(revoked)) => Ok(Err(StoreError::Superseded(revoked))),
                    Err(e) => Err(e),
                };
                (tx, landed)
            })
        })
        .await
    }

    async fn file(&self, repository: i64, path: &str) -> Result<Option<RawFile>, StoreError> {
        let row: Option<FileRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM raw_files WHERE repository_id = ?1 AND path = ?2"
        ))
        .bind(repository)
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(file_of).transpose()
    }

    async fn list(
        &self,
        repository: i64,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<RawFile>, StoreError> {
        let rows: Vec<FileRow> = if prefix.is_empty() {
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM raw_files WHERE repository_id = ?1 ORDER BY path LIMIT ?2"
            ))
            .bind(repository)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM raw_files
                 WHERE repository_id = ?1 AND (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                 ORDER BY path LIMIT ?4"
            ))
            .bind(repository)
            .bind(prefix)
            .bind(like_prefix(prefix))
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(store_error)?;
        rows.into_iter().map(file_of).collect()
    }

    async fn delete_file(
        &self,
        repository: i64,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = async {
                    let held: Option<String> = sqlx::query_scalar(
                        "SELECT physical_key FROM raw_files WHERE repository_id = ?1 AND path = ?2",
                    )
                    .bind(repository)
                    .bind(path)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let Some(key) = held else {
                        return Ok(Err(StoreError::NotFound));
                    };
                    sqlx::query("DELETE FROM raw_files WHERE repository_id = ?1 AND path = ?2")
                        .bind(repository)
                        .bind(path)
                        .execute(&mut *tx)
                        .await?;
                    let released = vec![key];
                    super::reclaim::enqueue_keys(&mut tx, &released, now).await?;
                    Ok(Ok(released))
                }
                .await;
                (tx, done)
            })
        })
        .await
    }
}
