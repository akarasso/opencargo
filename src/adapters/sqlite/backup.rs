//! Backup and restore of the SQLite file: `VACUUM INTO` for a consistent
//! copy of a live database, a truncating checkpoint, and the swap.

use std::path::Path;

use async_trait::async_trait;
use sqlx::SqlitePool;

use super::store_error;
use crate::error::StoreError;
use crate::ports::backup::{Checkpoint, DatabaseBackup, DatabaseFiles};

pub struct SqliteBackup {
    pool: SqlitePool,
}

impl SqliteBackup {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DatabaseBackup for SqliteBackup {
    async fn snapshot_into(&self, dest: &Path) -> Result<(), StoreError> {
        let dest = dest
            .to_str()
            .ok_or_else(|| StoreError::Other("a snapshot path must be UTF-8".into()))?;
        sqlx::query("VACUUM INTO ?1")
            .bind(dest)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn checkpoint(&self) -> Result<Checkpoint, StoreError> {
        let (busy, log, checkpointed): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(Checkpoint {
            busy: busy != 0,
            log_pages: log,
            checkpointed_pages: checkpointed,
        })
    }

    async fn size(&self) -> Result<u64, StoreError> {
        let bytes: i64 = sqlx::query_scalar(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(bytes.max(0) as u64)
    }
}

pub struct SqliteFiles;

#[async_trait]
impl DatabaseFiles for SqliteFiles {
    async fn verify(&self, file: &Path) -> Result<(), String> {
        let url = format!("sqlite:{}?mode=ro", file.display());
        let pool = SqlitePool::connect(&url).await.map_err(|e| e.to_string())?;
        let verdict: Result<String, _> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&pool)
            .await;
        pool.close().await;
        match verdict {
            Ok(ok) if ok == "ok" => Ok(()),
            Ok(problem) => Err(problem),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn replace(&self, db_path: &Path, snapshot: &Path) -> Result<(), StoreError> {
        let io = |e: std::io::Error| StoreError::Other(Box::new(e));
        for suffix in ["-wal", "-shm"] {
            let side = side_file(db_path, suffix);
            match tokio::fs::remove_file(&side).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
                _ => {}
            }
        }
        let staged = side_file(db_path, ".restoring");
        tokio::fs::copy(snapshot, &staged).await.map_err(io)?;
        tokio::fs::rename(&staged, db_path).await.map_err(io)?;
        self.verify(db_path)
            .await
            .map_err(|problem| StoreError::Other(format!("the restored database: {problem}").into()))
    }
}

fn side_file(db_path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::sqlite::connect;

    #[tokio::test]
    async fn a_snapshot_is_a_verified_copy_and_the_checkpoint_reports_its_columns() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pool = connect(&format!("sqlite:{}?mode=rwc", tmp.path().join("a.db").display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO t VALUES (1)").execute(&pool).await.unwrap();
        let backup = SqliteBackup::new(pool.clone());
        let copy = tmp.path().join("copy.db");
        backup.snapshot_into(&copy).await.unwrap();
        SqliteFiles.verify(&copy).await.unwrap();
        assert!(backup.size().await.unwrap() > 0);
        let idle = backup.checkpoint().await.unwrap();
        assert!(!idle.busy);
        std::fs::write(tmp.path().join("junk.db"), b"not a database at all, not even close").unwrap();
        assert!(SqliteFiles.verify(&tmp.path().join("junk.db")).await.is_err());
    }
}
