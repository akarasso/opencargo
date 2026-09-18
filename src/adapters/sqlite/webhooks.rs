//! `WebhookStore` over SQLite: one statement per method, and one row struct
//! that keeps the column encodings — `active` as an integer, the events as a
//! comma-separated list — on this side of the port.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use super::{bind_ts, corrupt_row, read_ts, store_error};
use crate::domain::{DomainError, Subscription, Webhook};
use crate::error::StoreError;
use crate::ports::webhooks::{NewWebhook, WebhookPatch, WebhookStore};

const COLUMNS: &str = "id, url, events, secret, active, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: i64,
    url: String,
    events: String,
    secret: Option<String>,
    active: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<WebhookRow> for Webhook {
    type Error = DomainError;

    fn try_from(row: WebhookRow) -> Result<Self, DomainError> {
        Ok(Webhook {
            events: Subscription::parse(&row.events),
            active: row.active != 0,
            created_at: read_ts(&row.url, "created_at", &row.created_at)?,
            updated_at: read_ts(&row.url, "updated_at", &row.updated_at)?,
            id: row.id,
            url: row.url,
            secret: row.secret,
        })
    }
}

fn decode(row: WebhookRow) -> Result<Webhook, StoreError> {
    Webhook::try_from(row).map_err(corrupt_row)
}

pub struct SqliteWebhookStore {
    pool: SqlitePool,
}

impl SqliteWebhookStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn row(&self, id: i64) -> Result<Option<Webhook>, StoreError> {
        let row: Option<WebhookRow> =
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM webhooks WHERE id = ?1"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        row.map(decode).transpose()
    }

    async fn rows(&self, predicate: &str) -> Result<Vec<Webhook>, StoreError> {
        let rows: Vec<WebhookRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM webhooks {predicate} ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        rows.into_iter().map(decode).collect()
    }
}

#[async_trait]
impl WebhookStore for SqliteWebhookStore {
    async fn all(&self) -> Result<Vec<Webhook>, StoreError> {
        self.rows("").await
    }

    async fn by_id(&self, id: i64) -> Result<Option<Webhook>, StoreError> {
        self.row(id).await
    }

    async fn create(
        &self,
        hook: &NewWebhook<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        let row: WebhookRow = sqlx::query_as(&format!(
            "INSERT INTO webhooks (url, events, secret, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4) RETURNING {COLUMNS}"
        ))
        .bind(hook.url)
        .bind(hook.events.encoded())
        .bind(hook.secret)
        .bind(bind_ts(now))
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        decode(row)
    }

    async fn update(
        &self,
        id: i64,
        patch: &WebhookPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        if !patch.touches_nothing() {
            let mut query: QueryBuilder<Sqlite> =
                QueryBuilder::new("UPDATE webhooks SET updated_at = ");
            query.push_bind(bind_ts(now));
            if let Some(url) = patch.url {
                query.push(", url = ").push_bind(url);
            }
            if let Some(events) = patch.events {
                query.push(", events = ").push_bind(events.encoded());
            }
            if let Some(secret) = patch.secret {
                query.push(", secret = ").push_bind(secret);
            }
            if let Some(active) = patch.active {
                query.push(", active = ").push_bind(i64::from(active));
            }
            query.push(" WHERE id = ").push_bind(id);
            query
                .build()
                .execute(&self.pool)
                .await
                .map_err(store_error)?;
        }
        self.row(id).await?.ok_or(StoreError::NotFound)
    }

    async fn delete(&self, id: i64) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM webhooks WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn active(&self) -> Result<Vec<Webhook>, StoreError> {
        self.rows("WHERE active = 1").await
    }

    async fn ensure_seeded(
        &self,
        hooks: &[NewWebhook<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if hooks.is_empty() {
            return Ok(());
        }
        let registered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhooks")
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;
        if registered > 0 {
            return Ok(());
        }
        for hook in hooks {
            self.create(hook, now).await?;
        }
        Ok(())
    }
}
