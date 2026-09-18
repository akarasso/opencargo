//! `UserStore` over SQLite: one row struct holding the column encodings —
//! text timestamps, an integer `must_change_password` — on this side of the
//! port, and one statement per method.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::domain::{DomainError, User};
use crate::error::StoreError;
use crate::ports::users::{NewUser, UserPatch, UserStore};

const COLUMNS: &str =
    "id, username, email, password_hash, role, must_change_password, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    username: String,
    email: Option<String>,
    password_hash: String,
    role: String,
    must_change_password: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<UserRow> for User {
    type Error = DomainError;

    fn try_from(row: UserRow) -> Result<Self, DomainError> {
        Ok(User {
            created_at: read_ts(&row.username, "created_at", &row.created_at)?,
            updated_at: read_ts(&row.username, "updated_at", &row.updated_at)?,
            must_change_password: row.must_change_password != 0,
            id: row.id,
            username: row.username,
            email: row.email,
            password_hash: row.password_hash,
            role: row.role,
        })
    }
}

fn decode(row: UserRow) -> Result<User, StoreError> {
    User::try_from(row).map_err(corrupt_row)
}

pub(crate) async fn by_id_in(tx: &mut super::Tx, id: i64) -> Result<Option<User>, sqlx::Error> {
    let row: Option<UserRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE id = ?1"))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.and_then(|r| decode(r).ok()))
}

/// `Conflict` if the name is taken, inside a caller's transaction.
pub(crate) async fn insert_in(
    tx: &mut super::Tx,
    user: &NewUser<'_>,
    now: DateTime<Utc>,
) -> Result<Result<User, StoreError>, sqlx::Error> {
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE username = ?1)")
        .bind(user.username)
        .fetch_one(&mut **tx)
        .await?;
    if taken {
        return Ok(Err(StoreError::Conflict));
    }
    let row: UserRow = sqlx::query_as(&format!(
        "INSERT INTO users (username, email, password_hash, role, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING {COLUMNS}"
    ))
    .bind(user.username)
    .bind(user.email)
    .bind(user.password_hash)
    .bind(user.role)
    .bind(bind_ts(now))
    .fetch_one(&mut **tx)
    .await?;
    Ok(decode(row))
}

pub struct SqliteUserStore {
    pool: SqlitePool,
}

impl SqliteUserStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserStore for SqliteUserStore {
    async fn by_name(&self, username: &str) -> Result<Option<User>, StoreError> {
        let row: Option<UserRow> =
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE username = ?1"))
                .bind(username)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        row.map(decode).transpose()
    }

    async fn by_id(&self, id: i64) -> Result<Option<User>, StoreError> {
        let row: Option<UserRow> =
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE id = ?1"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        row.map(decode).transpose()
    }

    async fn all(&self) -> Result<Vec<User>, StoreError> {
        let rows: Vec<UserRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM users ORDER BY id"))
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        rows.into_iter().map(decode).collect()
    }

    async fn create(&self, user: &NewUser<'_>, now: DateTime<Utc>) -> Result<User, StoreError> {
        let row: UserRow = sqlx::query_as(&format!(
            "INSERT INTO users (username, email, password_hash, role, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING {COLUMNS}"
        ))
        .bind(user.username)
        .bind(user.email)
        .bind(user.password_hash)
        .bind(user.role)
        .bind(bind_ts(now))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        decode(row)
    }

    async fn update(
        &self,
        username: &str,
        patch: &UserPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError> {
        if !patch.touches_nothing() {
            let mut query: QueryBuilder<Sqlite> =
                QueryBuilder::new("UPDATE users SET updated_at = ");
            query.push_bind(bind_ts(now));
            if let Some(email) = patch.email {
                query.push(", email = ").push_bind(email);
            }
            if let Some(hash) = patch.password_hash {
                query.push(", password_hash = ").push_bind(hash);
            }
            if let Some(role) = patch.role {
                query.push(", role = ").push_bind(role);
            }
            if let Some(must_change) = patch.must_change_password {
                query
                    .push(", must_change_password = ")
                    .push_bind(i64::from(must_change));
            }
            query.push(" WHERE username = ").push_bind(username);
            query
                .build()
                .execute(&self.pool)
                .await
                .map_err(store_error)?;
        }
        self.by_name(username).await?.ok_or(StoreError::NotFound)
    }

    async fn delete(&self, username: &str) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM users WHERE username = ?1")
            .bind(username)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
