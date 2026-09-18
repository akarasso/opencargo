//! `TokenStore` over SQLite. Expiry is never a predicate here: the rows come
//! back whole and the domain decides against the caller's clock, so no
//! statement compares a column against `datetime('now')`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::domain::{ApiToken, DomainError};
use crate::error::StoreError;
use crate::ports::tokens::{NewToken, TokenStore};

const COLUMNS: &str =
    "id, user_id, name, prefix, token_hash, expires_at, last_used_at, created_at";

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: String,
    user_id: i64,
    name: String,
    prefix: String,
    token_hash: String,
    expires_at: Option<String>,
    last_used_at: Option<String>,
    created_at: String,
}

impl TryFrom<TokenRow> for ApiToken {
    type Error = DomainError;

    fn try_from(row: TokenRow) -> Result<Self, DomainError> {
        let optional = |column, stored: &Option<String>| match stored {
            Some(stored) => read_ts(&row.id, column, stored).map(Some),
            None => Ok(None),
        };
        Ok(ApiToken {
            created_at: read_ts(&row.id, "created_at", &row.created_at)?,
            expires_at: optional("expires_at", &row.expires_at)?,
            last_used_at: optional("last_used_at", &row.last_used_at)?,
            id: row.id.clone(),
            user_id: row.user_id,
            name: row.name,
            prefix: row.prefix,
            token_hash: row.token_hash,
        })
    }
}

fn decode(row: TokenRow) -> Result<ApiToken, StoreError> {
    ApiToken::try_from(row).map_err(corrupt_row)
}

pub struct SqliteTokenStore {
    pool: SqlitePool,
}

impl SqliteTokenStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn one(&self, column: &str, key: &str) -> Result<Option<ApiToken>, StoreError> {
        let row: Option<TokenRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM api_tokens WHERE {column} = ?1"
        ))
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(decode).transpose()
    }
}

#[async_trait]
impl TokenStore for SqliteTokenStore {
    async fn by_prefix(&self, prefix: &str) -> Result<Option<ApiToken>, StoreError> {
        self.one("prefix", prefix).await
    }

    async fn by_id(&self, id: &str) -> Result<Option<ApiToken>, StoreError> {
        self.one("id", id).await
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<ApiToken>, StoreError> {
        let rows: Vec<TokenRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM api_tokens WHERE user_id = ?1 ORDER BY created_at DESC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter().map(decode).collect()
    }

    async fn create(
        &self,
        token: &NewToken<'_>,
        now: DateTime<Utc>,
    ) -> Result<ApiToken, StoreError> {
        let row: TokenRow = sqlx::query_as(&format!(
            "INSERT INTO api_tokens (id, user_id, name, prefix, token_hash, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING {COLUMNS}"
        ))
        .bind(token.id)
        .bind(token.user_id)
        .bind(token.name)
        .bind(token.prefix)
        .bind(token.token_hash)
        .bind(token.expires_at.map(bind_ts))
        .bind(bind_ts(now))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        decode(row)
    }

    async fn delete(&self, id: &str) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM api_tokens WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn touch(&self, id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2")
            .bind(bind_ts(now))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}
