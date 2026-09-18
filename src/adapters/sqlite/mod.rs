//! The SQLite adapter: the only place the dialect and the driver are named.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::StoreError;
use crate::ports::packages::PackageStore;
use crate::ports::repositories::RepositoryStore;
use crate::ports::search::SearchIndex;
use crate::ports::webhooks::WebhookStore;

pub mod migrate;
pub mod multipart;
pub mod packages;
pub mod repositories;
pub mod search;
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

/// What one attempt of a coarse method does. It is handed the open
/// transaction and hands it back, so a retry opens a fresh one rather than
/// reusing a poisoned handle.
///
/// The two error levels are not the same failure: the inner `StoreError` is a
/// refusal the caller asked for — a conflict, a missing row — which rolls the
/// transaction back and is final, while an outer `sqlx::Error` is the driver
/// failing, which [`immediate`] may retry.
pub(crate) type Tx = Transaction<'static, Sqlite>;
pub(crate) type TxStep<'a, T> = Pin<
    Box<dyn Future<Output = (Tx, Result<Result<T, StoreError>, sqlx::Error>)> + Send + 'a>,
>;

/// Attempts of a coarse method before a busy database is reported as such.
const ATTEMPTS: u32 = 3;

/// Run `act` inside `BEGIN IMMEDIATE`, retrying a busy database.
///
/// `IMMEDIATE` rather than sqlx's bare `BEGIN`: every coarse method here
/// reads and then writes, and in WAL a deferred transaction that tries to
/// upgrade after another connection committed gets `SQLITE_BUSY_SNAPSHOT` at
/// once — the pool's `busy_timeout` is never consulted, because waiting
/// cannot refresh a stale snapshot. Taking the write lock up front puts the
/// wait where the timeout does apply, and what survives it is a retry (503),
/// never an internal error (500).
pub(crate) async fn immediate<'a, T, F>(pool: &SqlitePool, mut act: F) -> Result<T, StoreError>
where
    F: FnMut(Tx) -> TxStep<'a, T> + Send,
    T: Send,
{
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match once(pool, &mut act).await {
            Ok(outcome) => return outcome,
            Err(err) if is_busy(&err) => {
                backoff(attempt).await;
                last = Some(err);
            }
            Err(err) => return Err(store_error(err)),
        }
    }
    Err(last.map_or(StoreError::Unavailable, store_error))
}

/// One transaction: `Err` is the driver failing, and the only thing the
/// caller above may retry.
async fn once<'a, T, F>(
    pool: &SqlitePool,
    act: &mut F,
) -> Result<Result<T, StoreError>, sqlx::Error>
where
    F: FnMut(Tx) -> TxStep<'a, T> + Send,
    T: Send,
{
    let tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    match act(tx).await {
        (tx, Ok(Ok(value))) => {
            tx.commit().await?;
            Ok(Ok(value))
        }
        (_, Ok(Err(refusal))) => Ok(Err(refusal)),
        (_, Err(err)) => Err(err),
    }
}

/// SQLITE_BUSY and SQLITE_BUSY_SNAPSHOT, as extended result codes.
fn is_busy(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => matches!(db.code().as_deref(), Some("5") | Some("517")),
        _ => false,
    }
}

/// Jittered, so two writers that collided do not collide again in step.
async fn backoff(attempt: u32) {
    let base = 10u64 << attempt;
    let jitter = rand::thread_rng().gen_range(0..=base);
    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
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

    pub fn repositories(&self) -> Arc<dyn RepositoryStore> {
        Arc::new(repositories::SqliteRepositoryStore::new(self.pool.clone()))
    }

    pub fn packages(&self) -> Arc<dyn PackageStore> {
        Arc::new(packages::SqlitePackageStore::new(self.pool.clone()))
    }

    pub fn search(&self) -> Arc<dyn SearchIndex> {
        Arc::new(search::SqliteSearchIndex::new(self.pool.clone()))
    }
}
