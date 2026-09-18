//! `SearchIndex` over SQLite's FTS5 table.
//!
//! Nothing here writes `packages_fts`: `007_fts5.sql`'s three triggers
//! maintain it from the `packages` writes, which is exactly what makes the
//! index this adapter's private business rather than a port method.
//!
//! There is no `LIKE` fallback. A query that FTS5 would refuse never reaches
//! here — [`SearchQuery::parse`] has already turned it into an empty answer —
//! so a failure from this table is a store failure, not a downgrade.

use async_trait::async_trait;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{public_packages, store_error};
use crate::db::PackageRow;
use crate::domain::Package;
use crate::error::StoreError;
use crate::ports::search::{SearchIndex, SearchQuery, SearchScope};

const COLUMNS: &str =
    "id, repository_id, name, description, readme, license, created_at, updated_at";

pub struct SqliteSearchIndex {
    pool: SqlitePool,
}

impl SqliteSearchIndex {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// The scope as a predicate over `p`, bound rather than spliced where it
/// carries a value.
fn scope_predicate(query: &mut QueryBuilder<'_, Sqlite>, scope: SearchScope) {
    match scope {
        SearchScope::Repo(repository) => {
            query.push("p.repository_id = ").push_bind(repository);
        }
        SearchScope::PublicOnly => {
            query.push(public_packages("p."));
        }
        SearchScope::All => {
            query.push("1 = 1");
        }
    }
}

/// FTS5's `MATCH` argument: every token quoted as a phrase, so `@scope/pkg`
/// and `left-pad` are searched for literally rather than as operators.
fn match_expression(query: &SearchQuery) -> String {
    query
        .tokens()
        .iter()
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl SearchIndex for SqliteSearchIndex {
    async fn search(
        &self,
        scope: SearchScope,
        query: Option<&SearchQuery>,
        limit: u32,
    ) -> Result<Vec<Package>, StoreError> {
        let mut sql: QueryBuilder<'_, Sqlite> =
            QueryBuilder::new(format!("SELECT {} FROM packages p", columns()));
        if query.is_some() {
            sql.push(" JOIN packages_fts fts ON p.id = fts.rowid");
        }
        sql.push(" WHERE ");
        scope_predicate(&mut sql, scope);
        if let Some(query) = query {
            sql.push(" AND packages_fts MATCH ")
                .push_bind(match_expression(query))
                .push(" ORDER BY rank");
        }
        sql.push(" LIMIT ").push_bind(i64::from(limit));

        let rows: Vec<PackageRow> = sql
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        rows.into_iter()
            .map(|row| Package::try_from(row).map_err(|err| StoreError::Other(Box::new(err))))
            .collect()
    }
}

/// `p.`-qualified, because the FTS join brings a second `name` into scope.
fn columns() -> String {
    COLUMNS
        .split(", ")
        .map(|column| format!("p.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}
