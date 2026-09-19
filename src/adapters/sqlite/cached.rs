//! `CachedPackageIndex` over `cached_packages` and its FTS5 table.
//!
//! Like `packages_fts`, the index is maintained by `027`'s triggers, so the
//! only statements here are over the base table.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, corrupt_row, public_packages, read_ts, store_error};
use crate::domain::{CachedPackage, DomainError, Sighting};
use crate::error::StoreError;
use crate::ports::search::{CachedPackageIndex, SearchQuery, SearchScope};

const COLUMNS: &str =
    "repository_id, format, name, description, latest_version, first_seen_at, last_seen_at";

#[derive(Debug, sqlx::FromRow)]
struct CachedRow {
    repository_id: i64,
    format: String,
    name: String,
    description: Option<String>,
    latest_version: Option<String>,
    first_seen_at: String,
    last_seen_at: String,
}

impl TryFrom<CachedRow> for CachedPackage {
    type Error = DomainError;

    fn try_from(row: CachedRow) -> Result<Self, DomainError> {
        Ok(CachedPackage {
            format: row.format.parse().map_err(|_| DomainError::CorruptColumn {
                repo: row.name.clone(),
                column: "format",
                value: row.format.clone(),
            })?,
            first_seen_at: read_ts(&row.name, "first_seen_at", &row.first_seen_at)?,
            last_seen_at: read_ts(&row.name, "last_seen_at", &row.last_seen_at)?,
            repository_id: row.repository_id,
            name: row.name,
            description: row.description,
            latest_version: row.latest_version,
        })
    }
}

pub struct SqliteCachedPackageIndex {
    pool: SqlitePool,
}

impl SqliteCachedPackageIndex {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn scope_predicate(query: &mut QueryBuilder<'_, Sqlite>, scope: SearchScope) {
    match scope {
        SearchScope::Repo(repository) => {
            query.push("c.repository_id = ").push_bind(repository);
        }
        SearchScope::PublicOnly => {
            query.push(public_packages("c."));
        }
        SearchScope::All => {
            query.push("1 = 1");
        }
    }
}

fn match_expression(query: &SearchQuery) -> String {
    query
        .tokens()
        .iter()
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn columns() -> String {
    COLUMNS
        .split(", ")
        .map(|column| format!("c.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl CachedPackageIndex for SqliteCachedPackageIndex {
    /// A sighting that says nothing new keeps what an earlier document said:
    /// an npm packument carries a description, a Cargo index line does not,
    /// and both may be the same package seen through two members.
    async fn remember(&self, seen: &Sighting<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO cached_packages
                 (repository_id, format, name, description, latest_version,
                  first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(repository_id, name) DO UPDATE SET
                 format = excluded.format,
                 description = COALESCE(excluded.description, cached_packages.description),
                 latest_version = COALESCE(excluded.latest_version, cached_packages.latest_version),
                 last_seen_at = excluded.last_seen_at",
        )
        .bind(seen.repository_id)
        .bind(seen.format.as_str())
        .bind(seen.name)
        .bind(seen.description)
        .bind(seen.latest_version)
        .bind(bind_ts(now))
        .bind(bind_ts(now))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn search(
        &self,
        scope: SearchScope,
        query: Option<&SearchQuery>,
        limit: u32,
    ) -> Result<Vec<CachedPackage>, StoreError> {
        let mut sql: QueryBuilder<'_, Sqlite> =
            QueryBuilder::new(format!("SELECT {} FROM cached_packages c", columns()));
        if query.is_some() {
            sql.push(" JOIN cached_packages_fts fts ON c.id = fts.rowid");
        }
        sql.push(" WHERE ");
        scope_predicate(&mut sql, scope);
        if let Some(query) = query {
            sql.push(" AND cached_packages_fts MATCH ")
                .push_bind(match_expression(query))
                .push(" ORDER BY rank");
        }
        sql.push(" LIMIT ").push_bind(i64::from(limit));

        let rows: Vec<CachedRow> = sql
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        rows.into_iter()
            .map(|row| CachedPackage::try_from(row).map_err(corrupt_row))
            .collect()
    }

    async fn forget_repo(&self, repo: i64) -> Result<u64, StoreError> {
        let done = sqlx::query("DELETE FROM cached_packages WHERE repository_id = ?")
            .bind(repo)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(done.rows_affected())
    }
}
