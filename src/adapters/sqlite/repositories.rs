//! `RepositoryStore` over SQLite.
//!
//! `delete_empty` is the one transaction here. `proxy_cache_entries` and the
//! grants cascade through their foreign keys; `proxy_cache_meta` (declared
//! before the tree used `ON DELETE CASCADE`) does not, so it goes explicitly.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, immediate, store_error, Tx};
use crate::adapters::sqlite::rows::RepositoryRow;
use crate::domain::{RepoSpec, Repository};
use crate::error::StoreError;
use crate::ports::repositories::{RepoPatch, RepositoryStore};

const COLUMNS: &str =
    "id, name, repo_type, format, visibility, upstream_url, config_json, created_at, updated_at";

pub struct SqliteRepositoryStore {
    pool: SqlitePool,
}

impl SqliteRepositoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn row(&self, name: &str) -> Result<Option<Repository>, StoreError> {
        let row: Option<RepositoryRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repositories WHERE name = ?1"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        decode(row.into_iter()).map(|mut all| all.pop())
    }
}

/// Rows in the domain's vocabulary; an unreadable column is corrupt, never a
/// silent fallback.
fn decode(
    rows: impl Iterator<Item = RepositoryRow>,
) -> Result<Vec<Repository>, StoreError> {
    rows.map(|row| Repository::try_from(row).map_err(|err| StoreError::Other(Box::new(err))))
        .collect()
}

/// The conflict check, the cache rows the schema does not cascade, and the
/// row itself.
async fn drop_empty(
    tx: &mut Tx,
    name: &str,
) -> Result<Result<(), StoreError>, sqlx::Error> {
    let repo: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE name = ?1")
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(Err(StoreError::NotFound));
    };

    let packages: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM packages WHERE repository_id = ?1")
            .bind(repo)
            .fetch_one(&mut **tx)
            .await?;
    if packages > 0 {
        return Ok(Err(StoreError::Conflict));
    }

    sqlx::query("DELETE FROM proxy_cache_meta WHERE repository_id = ?1")
        .bind(repo)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM repositories WHERE id = ?1")
        .bind(repo)
        .execute(&mut **tx)
        .await?;
    Ok(Ok(()))
}

#[async_trait]
impl RepositoryStore for SqliteRepositoryStore {
    async fn by_name(&self, name: &str) -> Result<Option<Repository>, StoreError> {
        self.row(name).await
    }

    async fn by_id(&self, id: i64) -> Result<Option<Repository>, StoreError> {
        let row: Option<RepositoryRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repositories WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        decode(row.into_iter()).map(|mut all| all.pop())
    }

    async fn all(&self) -> Result<Vec<Repository>, StoreError> {
        let rows: Vec<RepositoryRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repositories ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        decode(rows.into_iter())
    }

    async fn names(&self) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar("SELECT name FROM repositories ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)
    }

    async fn create(
        &self,
        spec: &RepoSpec<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError> {
        let row: RepositoryRow = sqlx::query_as(&format!(
            "INSERT INTO repositories
                 (name, repo_type, format, visibility, upstream_url, config_json,
                  created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) RETURNING {COLUMNS}"
        ))
        .bind(spec.name)
        .bind(spec.kind.as_str())
        .bind(spec.format.as_str())
        .bind(spec.visibility.as_str())
        .bind(spec.upstream)
        .bind(spec.config().map(|config| config.to_json()))
        .bind(bind_ts(now))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        decode(std::iter::once(row))?
            .pop()
            .ok_or(StoreError::NotFound)
    }

    async fn update(
        &self,
        name: &str,
        patch: &RepoPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError> {
        if !patch.touches_nothing() {
            let mut query: QueryBuilder<Sqlite> =
                QueryBuilder::new("UPDATE repositories SET updated_at = ");
            query.push_bind(bind_ts(now));
            if let Some(visibility) = patch.visibility {
                query.push(", visibility = ").push_bind(visibility.as_str());
            }
            if let Some(upstream) = patch.upstream {
                query.push(", upstream_url = ").push_bind(upstream);
            }
            if let Some(config) = patch.config {
                query.push(", config_json = ").push_bind(config.to_json());
            }
            query.push(" WHERE name = ").push_bind(name);
            query
                .build()
                .execute(&self.pool)
                .await
                .map_err(store_error)?;
        }
        self.row(name).await?.ok_or(StoreError::NotFound)
    }

    async fn delete_empty(&self, name: &str) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let dropped = drop_empty(&mut tx, name).await;
                (tx, dropped)
            })
        })
        .await
    }

    async fn ensure_seeded(
        &self,
        specs: &[RepoSpec<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        for spec in specs {
            sqlx::query(
                "INSERT OR IGNORE INTO repositories
                     (name, repo_type, format, visibility, upstream_url, config_json,
                      created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            )
            .bind(spec.name)
            .bind(spec.kind.as_str())
            .bind(spec.format.as_str())
            .bind(spec.visibility.as_str())
            .bind(spec.upstream)
            .bind(spec.config().map(|config| config.to_json()))
            .bind(bind_ts(now))
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::sqlite::SqliteStores;
    use crate::app::repo_spec::check_repository_names;
    use crate::domain::{Format, RepoKind, Visibility};

    /// The startup guard reads names and nothing else, so a column no row
    /// decoder accepts cannot mask the report of the name it is about — the
    /// report is the only thing that tells the operator what to rename.
    #[tokio::test]
    async fn a_corrupt_column_does_not_hide_an_offending_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stores = SqliteStores::open(&tmp.path().join("t.db")).await.unwrap();
        let repos = stores.repositories();
        repos
            .create(
                &RepoSpec {
                    name: "a/b",
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE repositories SET created_at = 'not a time'")
            .execute(&stores.pool())
            .await
            .unwrap();

        assert!(repos.all().await.is_err(), "the row no longer decodes");
        let err = check_repository_names(repos.as_ref())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("a/b"), "{err}");
    }
}
