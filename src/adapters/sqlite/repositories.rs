//! `RepositoryStore` over SQLite.
//!
//! `create` and `retire` are the transactions here. `create` allocates the
//! incarnation and records its prefixes; `retire` re-checks both conflicts,
//! removes the row (grants and cache entries cascade, `proxy_cache_meta`
//! goes explicitly), marks the incarnation retired, revokes every pin under
//! its prefixes and enqueues them, all in one transaction.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, immediate, reclaim, store_error, Tx};
use crate::adapters::sqlite::rows::RepositoryRow;
use crate::domain::layout;
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

/// The repository row, its incarnation and the prefixes its bytes live
/// under: the incarnation's own, and the name-keyed ones writers still use.
async fn insert(
    tx: &mut Tx,
    spec: &RepoSpec<'_>,
    now: DateTime<Utc>,
) -> Result<Result<RepositoryRow, StoreError>, sqlx::Error> {
    let legacy_prefixes = layout::name_keyed_prefixes(spec.format.as_str(), spec.name);
    for prefix in &legacy_prefixes {
        let claimed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM reclaim_claims WHERE key = ?1 AND until > ?2)",
        )
        .bind(prefix)
        .bind(bind_ts(now))
        .fetch_one(&mut **tx)
        .await?;
        if claimed {
            return Ok(Err(StoreError::Conflict));
        }
    }
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
    .fetch_one(&mut **tx)
    .await?;
    let incarnation = uuid::Uuid::new_v4().simple().to_string();
    sqlx::query("INSERT INTO repository_incarnations (repository_id, incarnation) VALUES (?1, ?2)")
        .bind(row.id)
        .bind(&incarnation)
        .execute(&mut **tx)
        .await?;
    let own = std::iter::once((layout::incarnation_prefix(&incarnation), 0));
    for prefix in &legacy_prefixes {
        sqlx::query("DELETE FROM reclaim_candidates WHERE key = ?1 AND prefix = 1")
            .bind(prefix)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM reclaim_claims WHERE key = ?1")
            .bind(prefix)
            .execute(&mut **tx)
            .await?;
    }
    let legacy = legacy_prefixes.into_iter().map(|p| (p, 1));
    for (prefix, legacy) in own.chain(legacy) {
        sqlx::query(
            "INSERT INTO storage_prefixes (prefix, incarnation, legacy) VALUES (?1, ?2, ?3)
             ON CONFLICT(prefix) DO UPDATE SET incarnation = excluded.incarnation,
                                               legacy = excluded.legacy",
        )
        .bind(&prefix)
        .bind(&incarnation)
        .bind(legacy)
        .execute(&mut **tx)
        .await?;
    }
    Ok(Ok(row))
}

/// The groups listing `name` as a member, read inside the transaction.
async fn holders(tx: &mut Tx, name: &str) -> Result<Result<Vec<String>, StoreError>, sqlx::Error> {
    let rows: Vec<RepositoryRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM repositories WHERE repo_type = 'group'"
    ))
    .fetch_all(&mut **tx)
    .await?;
    let groups = match decode(rows.into_iter()) {
        Ok(groups) => groups,
        Err(err) => return Ok(Err(err)),
    };
    Ok(Ok(groups
        .into_iter()
        .filter(|g| g.members().iter().any(|m| m == name))
        .map(|g| g.name)
        .collect()))
}

async fn retire_row(
    tx: &mut Tx,
    name: &str,
    now: DateTime<Utc>,
) -> Result<Result<Vec<String>, StoreError>, sqlx::Error> {
    let repo: Option<(i64, String)> =
        sqlx::query_as("SELECT id, repo_type FROM repositories WHERE name = ?1")
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((repo, kind)) = repo else {
        return Ok(Err(StoreError::NotFound));
    };
    let packages: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM packages WHERE repository_id = ?1")
            .bind(repo)
            .fetch_one(&mut **tx)
            .await?;
    let maven: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM maven_values WHERE repository_id = ?1")
            .bind(repo)
            .fetch_one(&mut **tx)
            .await?;
    let raw: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_files WHERE repository_id = ?1")
        .bind(repo)
        .fetch_one(&mut **tx)
        .await?;
    if packages > 0 || maven > 0 || raw > 0 {
        return Ok(Err(StoreError::Conflict));
    }
    match holders(tx, name).await? {
        Ok(groups) if groups.is_empty() => {}
        Ok(_) => return Ok(Err(StoreError::Conflict)),
        Err(err) => return Ok(Err(err)),
    }
    let incarnation: Option<String> = sqlx::query_scalar(
        "SELECT incarnation FROM repository_incarnations WHERE repository_id = ?1",
    )
    .bind(repo)
    .fetch_optional(&mut **tx)
    .await?;

    sqlx::query("DELETE FROM proxy_cache_meta WHERE repository_id = ?1")
        .bind(repo)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM repositories WHERE id = ?1")
        .bind(repo)
        .execute(&mut **tx)
        .await?;

    let Some(incarnation) = incarnation else {
        return Ok(Ok(Vec::new()));
    };
    sqlx::query(
        "INSERT INTO retired_incarnations (incarnation, retired_at) VALUES (?1, ?2)
         ON CONFLICT(incarnation) DO NOTHING",
    )
    .bind(&incarnation)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    let prefixes: Vec<String> =
        sqlx::query_scalar("SELECT prefix FROM storage_prefixes WHERE incarnation = ?1 ORDER BY prefix")
            .bind(&incarnation)
            .fetch_all(&mut **tx)
            .await?;
    for prefix in &prefixes {
        reclaim::revoke_under(tx, prefix).await?;
    }
    if kind == "group" {
        return Ok(Ok(Vec::new()));
    }
    for prefix in &prefixes {
        reclaim::enqueue_prefix(tx, prefix, now).await?;
    }
    Ok(Ok(prefixes))
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
        let row = immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let row = insert(&mut tx, spec, now).await;
                (tx, row)
            })
        })
        .await?;
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

    async fn retire(&self, name: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let retired = retire_row(&mut tx, name, now).await;
                (tx, retired)
            })
        })
        .await
    }

    async fn incarnation(&self, repository: i64) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT incarnation FROM repository_incarnations WHERE repository_id = ?1",
        )
        .bind(repository)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn ensure_seeded(
        &self,
        specs: &[RepoSpec<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        for spec in specs {
            immediate(&self.pool, |mut tx| {
                Box::pin(async move {
                    let done = async {
                        let present: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM repositories WHERE name = ?1)",
                        )
                        .bind(spec.name)
                        .fetch_one(&mut *tx)
                        .await?;
                        if !present {
                            if let Err(err) = insert(&mut tx, spec, now).await? {
                                return Ok(Err(err));
                            }
                        }
                        Ok(Ok(()))
                    }
                    .await;
                    (tx, done)
                })
            })
            .await?;
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
