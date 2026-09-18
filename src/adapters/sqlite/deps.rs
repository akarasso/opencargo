//! `DependencyStore` over SQLite.
//!
//! `dependents` joins back through `versions` and `packages` because a
//! dependent is a package *at a version*, and the visibility gate is a
//! sub-select rather than a spliced predicate the caller could widen.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, store_error};
use crate::error::StoreError;
use crate::ports::deps::{Dependency, DependencyStore, Dependent, NewDependency};

#[derive(sqlx::FromRow)]
struct DependencyRow {
    dependency_name: String,
    dependency_version_req: String,
    dependency_type: String,
}

impl From<DependencyRow> for Dependency {
    fn from(row: DependencyRow) -> Self {
        Dependency {
            name: row.dependency_name,
            requirement: row.dependency_version_req,
            kind: row.dependency_type,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DependentRow {
    name: String,
    version: String,
}

impl From<DependentRow> for Dependent {
    fn from(row: DependentRow) -> Self {
        Dependent {
            name: row.name,
            version: row.version,
        }
    }
}

pub struct SqliteDependencyStore {
    pool: SqlitePool,
}

impl SqliteDependencyStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DependencyStore for SqliteDependencyStore {
    async fn record(
        &self,
        dep: &NewDependency<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO package_dependencies
                 (package_id, version_id, dependency_name, dependency_version_req,
                  dependency_type, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(dep.package)
        .bind(dep.version)
        .bind(dep.name)
        .bind(dep.requirement)
        .bind(dep.kind)
        .bind(bind_ts(now))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn of_version(&self, version: i64) -> Result<Vec<Dependency>, StoreError> {
        let rows: Vec<DependencyRow> = sqlx::query_as(
            "SELECT dependency_name, dependency_version_req, dependency_type
             FROM package_dependencies WHERE version_id = ?1 ORDER BY id",
        )
        .bind(version)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(Dependency::from).collect())
    }

    async fn dependents(
        &self,
        name: &str,
        public_only: bool,
    ) -> Result<Vec<Dependent>, StoreError> {
        let visible = if public_only {
            " AND p.repository_id IN (SELECT id FROM repositories WHERE visibility = 'public')"
        } else {
            ""
        };
        let rows: Vec<DependentRow> = sqlx::query_as(&format!(
            "SELECT DISTINCT p.name, v.version FROM package_dependencies d \
             JOIN versions v ON d.version_id = v.id \
             JOIN packages p ON d.package_id = p.id \
             WHERE d.dependency_name = ?1{visible}"
        ))
        .bind(name)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(Dependent::from).collect())
    }
}
