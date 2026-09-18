//! The SQLite adapter: the only place the dialect and the driver are named.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::domain::DomainError;
use crate::error::StoreError;
use crate::ports::permissions::PermissionStore;
use crate::ports::tokens::TokenStore;
use crate::ports::users::UserStore;
use crate::ports::webhooks::WebhookStore;

pub mod migrate;
pub mod multipart;
pub mod permissions;
pub mod tokens;
pub mod users;
pub mod webhooks;

/// This adapter's stored timestamp: UTC, second precision.
///
/// It is the format the 23 columns defaulting to `datetime('now')` are written
/// in, and two live predicates compare such columns lexicographically — an
/// adapter that wrote RFC 3339 instead would sort `'T'` above `' '` and
/// mis-evaluate every legacy row. Every statement here binds it, so no column
/// default ever fires and the row carries the caller's clock.
pub(crate) fn bind_ts(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `bind_ts`'s inverse, and the only place a stored timestamp is read: a
/// column the schema was supposed to constrain coming back unreadable names
/// itself rather than falling back to a value nobody wrote.
pub(crate) fn read_ts(
    subject: &str,
    column: &'static str,
    stored: &str,
) -> Result<DateTime<Utc>, DomainError> {
    crate::db::parse_ts(stored).ok_or_else(|| DomainError::CorruptColumn {
        repo: subject.to_string(),
        column,
        value: stored.to_string(),
    })
}

/// An unreadable column is the store's failure to answer, not a refusal the
/// caller can act on: it reaches the client as a 500 and the column reaches
/// the log.
pub(crate) fn corrupt_row(err: DomainError) -> StoreError {
    StoreError::Other(Box::new(err))
}

/// The driver's failures in the store's vocabulary, for every store here.
/// `Unavailable` is the one that must not collapse into `Other`: SQLite has a
/// single writer, so a busy database is a retry (503), never an internal
/// error (500).
pub(crate) fn store_error(err: sqlx::Error) -> StoreError {
    if let sqlx::Error::Database(ref db) = err {
        if db.is_unique_violation() {
            return StoreError::Conflict;
        }
        // SQLITE_BUSY and SQLITE_BUSY_SNAPSHOT, as extended result codes.
        if matches!(db.code().as_deref(), Some("5") | Some("517")) {
            return StoreError::Unavailable;
        }
    }
    StoreError::Other(Box::new(err))
}

/// Every store this adapter implements, over one pool.
///
/// Handing out the handles rather than the pool is what lets a caller — the
/// composition root, or the contract suite proving this adapter and a fake
/// answer alike — hold the SQLite side of the boundary without naming a pool
/// type of its own.
#[derive(Clone)]
pub struct SqliteStores {
    pool: SqlitePool,
}

impl SqliteStores {
    /// Over a pool the caller already opened: the composition root's way in.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// A database of its own, migrated: the way in for anything that owns no
    /// pool, such as the contract suite.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = crate::db::connect(&format!("sqlite:{}?mode=rwc", path.display())).await?;
        migrate::run_all(&pool).await?;
        Ok(Self::new(pool))
    }

    pub fn webhooks(&self) -> Arc<dyn WebhookStore> {
        Arc::new(webhooks::SqliteWebhookStore::new(self.pool.clone()))
    }

    pub fn users(&self) -> Arc<dyn UserStore> {
        Arc::new(users::SqliteUserStore::new(self.pool.clone()))
    }

    pub fn tokens(&self) -> Arc<dyn TokenStore> {
        Arc::new(tokens::SqliteTokenStore::new(self.pool.clone()))
    }

    pub fn permissions(&self) -> Arc<dyn PermissionStore> {
        Arc::new(permissions::SqlitePermissionStore::new(self.pool.clone()))
    }
}
