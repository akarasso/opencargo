//! The SQLite adapter: the only place the dialect and the driver are named.

use std::path::Path;
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::error::StoreError;
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::ports::webhooks::WebhookStore;

pub mod migrate;
pub mod multipart;
pub mod proxy_cache;
pub mod webhooks;

/// This adapter's stored timestamp: UTC, second precision.
///
/// It is the format the 23 columns defaulting to `datetime('now')` are written
/// in, and two live predicates compare such columns lexicographically — an
/// adapter that wrote RFC 3339 instead would sort `'T'` above `' '` and
/// mis-evaluate every legacy row. Every statement here binds it, so no column
/// default ever fires and the row carries the caller's clock.
///
/// It is defined next to its inverse `parse_ts`, one module below, which the
/// statements not yet behind a store still reach; both move here when they do.
pub(crate) use crate::db::{bind_ts, parse_ts};

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

    pub fn proxy_cache(&self) -> Arc<dyn ProxyCacheStore> {
        Arc::new(proxy_cache::SqliteProxyCacheStore::new(self.pool.clone()))
    }
}
