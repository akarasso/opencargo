//! `PermissionStore` over SQLite: the four `can_*` integers become one
//! [`Rights`] value on this side of the port, and the repository's name comes
//! back with the grant through a left join rather than a second lookup — the
//! grant survives its repository, and the screen has to say so.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, store_error};
use crate::domain::Rights;
use crate::error::StoreError;
use crate::ports::permissions::{PermissionStore, RepoRights};

#[derive(sqlx::FromRow)]
struct RightsRow {
    can_read: i64,
    can_write: i64,
    can_delete: i64,
    can_admin: i64,
}

impl From<RightsRow> for Rights {
    fn from(row: RightsRow) -> Self {
        Rights {
            read: row.can_read != 0,
            write: row.can_write != 0,
            delete: row.can_delete != 0,
            admin: row.can_admin != 0,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GrantRow {
    repository_id: i64,
    repository: Option<String>,
    #[sqlx(flatten)]
    rights: RightsRow,
}

impl From<GrantRow> for RepoRights {
    fn from(row: GrantRow) -> Self {
        RepoRights {
            repository_id: row.repository_id,
            repository: row.repository,
            rights: row.rights.into(),
        }
    }
}

pub struct SqlitePermissionStore {
    pool: SqlitePool,
}

impl SqlitePermissionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PermissionStore for SqlitePermissionStore {
    async fn rights(
        &self,
        user_id: i64,
        repository_id: i64,
    ) -> Result<Option<Rights>, StoreError> {
        let row: Option<RightsRow> = sqlx::query_as(
            "SELECT can_read, can_write, can_delete, can_admin FROM user_permissions
             WHERE user_id = ?1 AND repository_id = ?2",
        )
        .bind(user_id)
        .bind(repository_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(Rights::from))
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<RepoRights>, StoreError> {
        let rows: Vec<GrantRow> = sqlx::query_as(
            "SELECT p.repository_id, r.name AS repository,
                    p.can_read, p.can_write, p.can_delete, p.can_admin
             FROM user_permissions p
             LEFT JOIN repositories r ON r.id = p.repository_id
             WHERE p.user_id = ?1 ORDER BY p.id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows.into_iter().map(RepoRights::from).collect())
    }

    async fn set(
        &self,
        user_id: i64,
        repository_id: i64,
        rights: Rights,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO user_permissions
             (user_id, repository_id, can_read, can_write, can_delete, can_admin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(user_id, repository_id) DO UPDATE SET
               can_read = excluded.can_read,
               can_write = excluded.can_write,
               can_delete = excluded.can_delete,
               can_admin = excluded.can_admin",
        )
        .bind(user_id)
        .bind(repository_id)
        .bind(i64::from(rights.read))
        .bind(i64::from(rights.write))
        .bind(i64::from(rights.delete))
        .bind(i64::from(rights.admin))
        .bind(bind_ts(now))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn revoke(&self, user_id: i64, repository_id: i64) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM user_permissions WHERE user_id = ?1 AND repository_id = ?2")
            .bind(user_id)
            .bind(repository_id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}
