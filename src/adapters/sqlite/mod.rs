#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
//! The SQLite adapter: the only place the dialect and the driver are named.
//!
//! The allow above is the boundary's one legal silencing of those two lints,
//! and it means exactly that: axum is deliberately off both lists, so a file
//! carrying it is the persistence adapter and nothing else (4.1).

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tracing::info;

use crate::domain::DomainError;
use crate::error::StoreError;
use crate::ports::audit::AuditStore;
use crate::ports::dashboard::DashboardRead;
use crate::ports::deps::DependencyStore;
use crate::ports::multipart::MultipartLedger;
use crate::ports::maven::MavenFileStore;
use crate::ports::handoffs::LoginHandoffStore;
use crate::ports::identities::IdentityStore;
use crate::ports::leases::{LeaseStore, ServerStateStore};
use crate::ports::secrets::ServerSecretStore;
use crate::ports::oci::OciStore;
use crate::ports::packages::PackageStore;
use crate::ports::permissions::PermissionStore;
use crate::ports::policy::PolicyStore;
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::ports::pypi::PypiFileStore;
use crate::ports::reclaim::ReclaimStore;
use crate::ports::referenced::ReferencedKeys;
use crate::ports::repositories::RepositoryStore;
use crate::ports::search::SearchIndex;
use crate::ports::tokens::TokenStore;
use crate::ports::users::UserStore;
use crate::ports::vulns::VulnStore;
use crate::ports::webhooks::WebhookStore;

pub mod audit;
pub mod backup;
pub mod dashboard;
pub mod deps;
pub mod maven;
pub mod identities;
pub mod leases;
pub mod migrate;
pub mod multipart;
pub mod nuget;
pub mod oci;
pub mod packages;
pub mod permissions;
pub mod policy;
pub mod proxy_cache;
pub mod pypi;
pub mod reclaim;
pub mod rebuild;
pub mod repositories;
pub mod rows;
pub mod search;
pub mod tokens;
pub mod users;
pub mod vulns;
pub mod webhooks;

/// This adapter's stored timestamp: UTC, second precision.
///
/// It is the format the 23 columns defaulting to `datetime('now')` are written
/// in, and two live predicates compare such columns lexicographically — an
/// adapter that wrote RFC 3339 instead would sort `'T'` above `' '` and
/// mis-evaluate every legacy row. Every statement here binds it, so no column
/// default ever fires and the row carries the caller's clock.
///
/// It is defined next to its inverse `parse_ts`, in `rows` beside the row
/// structs that are the other half of the same codec.
pub(crate) use rows::{bind_ts, parse_ts};

/// `bind_ts`'s inverse, and the only place a stored timestamp is read: a
/// column the schema was supposed to constrain coming back unreadable names
/// itself rather than falling back to a value nobody wrote.
pub(crate) fn read_ts(
    subject: &str,
    column: &'static str,
    stored: &str,
) -> Result<DateTime<Utc>, DomainError> {
    parse_ts(stored).ok_or_else(|| DomainError::CorruptColumn {
        repo: subject.to_string(),
        column,
        value: stored.to_string(),
    })
}

/// Create a connection pool and enable WAL mode.
pub async fn connect(url: &str) -> anyhow::Result<SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
    use std::str::FromStr;

    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // Wait for a busy writer instead of failing immediately: instant
        // SQLITE_BUSY errors under load used to surface as spurious 401s in
        // the auth middleware.
        .busy_timeout(std::time::Duration::from_secs(5))
        // SQLite leaves foreign-key enforcement OFF per connection unless
        // asked; the schema declares FK constraints and relies on them.
        .pragma("foreign_keys", "ON");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    info!("Connected to SQLite database");
    Ok(pool)
}

/// The rows of `packages` a caller without the run of the place may see,
/// qualified by the alias its query gave the table (`""` when it gave none).
/// Written once because the dashboard's panels and the search index apply the
/// same rule, and two spellings of it would be two rules.
pub(crate) fn public_packages(alias: &str) -> String {
    format!("{alias}repository_id IN (SELECT id FROM repositories WHERE visibility = 'public')")
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

    /// What these stores are over, for this adapter's own tests: one of them
    /// corrupts a column no port can write.
    #[cfg(test)]
    pub(crate) fn pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// A database of its own, migrated: the way in for anything that owns no
    /// pool, such as the contract suite.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let pool = connect(&format!("sqlite:{}?mode=rwc", path.display())).await?;
        migrate::run_all(&pool).await?;
        Ok(Self::new(pool))
    }

    pub fn webhooks(&self) -> Arc<dyn WebhookStore> {
        Arc::new(webhooks::SqliteWebhookStore::new(self.pool.clone()))
    }

    pub fn proxy_cache(&self) -> Arc<dyn ProxyCacheStore> {
        Arc::new(proxy_cache::SqliteProxyCacheStore::new(self.pool.clone()))
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

    pub fn repositories(&self) -> Arc<dyn RepositoryStore> {
        Arc::new(repositories::SqliteRepositoryStore::new(self.pool.clone()))
    }

    pub fn packages(&self) -> Arc<dyn PackageStore> {
        Arc::new(packages::SqlitePackageStore::new(self.pool.clone()))
    }

    pub fn nuget_feed(&self) -> Arc<dyn crate::ports::nuget::NugetFeedRead> {
        Arc::new(nuget::SqliteNugetFeed::new(self.pool.clone()))
    }

    pub fn search(&self) -> Arc<dyn SearchIndex> {
        Arc::new(search::SqliteSearchIndex::new(self.pool.clone()))
    }

    pub fn oci(&self) -> Arc<dyn OciStore> {
        Arc::new(oci::SqliteOciStore::new(self.pool.clone()))
    }

    pub fn audit(&self) -> Arc<dyn AuditStore> {
        Arc::new(audit::SqliteAuditStore::new(self.pool.clone()))
    }

    pub fn dependencies(&self) -> Arc<dyn DependencyStore> {
        Arc::new(deps::SqliteDependencyStore::new(self.pool.clone()))
    }

    pub fn vulns(&self) -> Arc<dyn VulnStore> {
        Arc::new(vulns::SqliteVulnStore::new(self.pool.clone()))
    }

    pub fn policy(&self) -> Arc<dyn PolicyStore> {
        Arc::new(policy::SqlitePolicyStore::new(self.pool.clone()))
    }

    pub fn multipart(&self) -> Arc<dyn MultipartLedger> {
        Arc::new(multipart::SqliteMultipartLedger::new(self.pool.clone()))
    }

    pub fn pypi(&self) -> Arc<dyn PypiFileStore> {
        Arc::new(pypi::SqlitePypiFileStore::new(self.pool.clone()))
    }

    pub fn reclaim(&self) -> Arc<dyn ReclaimStore> {
        Arc::new(reclaim::SqliteReclaimStore::new(self.pool.clone()))
    }

    pub fn referenced(&self) -> Arc<dyn ReferencedKeys> {
        Arc::new(reclaim::SqliteReferencedKeys::new(self.pool.clone()))
    }

    pub fn maven(&self) -> Arc<dyn MavenFileStore> {
        Arc::new(maven::SqliteMavenFileStore::new(self.pool.clone()))
    }

    pub fn identities(&self) -> Arc<dyn IdentityStore> {
        Arc::new(identities::SqliteIdentityStore::new(self.pool.clone()))
    }

    pub fn handoffs(&self) -> Arc<dyn LoginHandoffStore> {
        Arc::new(identities::SqliteHandoffStore::new(self.pool.clone()))
    }

    pub fn secrets(&self) -> Arc<dyn ServerSecretStore> {
        Arc::new(identities::SqliteSecretStore::new(self.pool.clone()))
    }

    pub fn backup(&self) -> Arc<dyn crate::ports::backup::DatabaseBackup> {
        Arc::new(backup::SqliteBackup::new(self.pool.clone()))
    }

    /// Closes the pool, so nothing holds the database file open.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub fn leases(&self) -> Arc<dyn LeaseStore> {
        Arc::new(leases::SqliteLeaseStore::new(self.pool.clone()))
    }

    pub fn server_state(&self) -> Arc<dyn ServerStateStore> {
        Arc::new(leases::SqliteServerState::new(self.pool.clone()))
    }

    /// Every migration this binary carries, on these stores' database.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        migrate::run_all(&self.pool).await.map(|_| ())
    }

    pub fn dashboard(&self) -> Arc<dyn DashboardRead> {
        Arc::new(dashboard::SqliteDashboardRead::new(self.pool.clone()))
    }
}
