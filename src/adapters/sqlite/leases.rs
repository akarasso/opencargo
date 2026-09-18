//! The writer lease and the server's state rows over SQLite (022).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, read_ts, store_error};
use crate::error::StoreError;
use crate::ports::leases::{Acquired, LeaseRow, LeaseStore, ServerStateStore};

const MIGRATION: &str = include_str!("migrations/022_server_leases.sql");

/// The `server_leases` statement of 022, verbatim: the one definition both
/// the migration and the pre-migration `ensure` run.
pub fn lease_table_statement() -> &'static str {
    MIGRATION
        .split(';')
        .map(str::trim)
        .find(|s| s.contains("server_leases"))
        .expect("022 creates server_leases")
}

pub struct SqliteLeaseStore {
    pool: SqlitePool,
}

impl SqliteLeaseStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

type Row = (String, String, String, String, String);

fn lease_row((name, owner, version, acquired_at, renewed_at): Row) -> Result<LeaseRow, StoreError> {
    let decode = |column, stored: &str| read_ts(&name, column, stored).map_err(super::corrupt_row);
    Ok(LeaseRow {
        acquired_at: decode("acquired_at", &acquired_at)?,
        renewed_at: decode("renewed_at", &renewed_at)?,
        name,
        owner,
        version,
    })
}

#[async_trait]
impl LeaseStore for SqliteLeaseStore {
    async fn ensure(&self) -> Result<(), StoreError> {
        sqlx::query(lease_table_statement())
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn acquire(
        &self,
        name: &str,
        owner: &str,
        version: &str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> Result<Acquired, StoreError> {
        let stale = chrono::Duration::from_std(stale_after).unwrap_or(chrono::Duration::MAX);
        let before = now.checked_sub_signed(stale).unwrap_or(DateTime::<Utc>::MIN_UTC);
        let taken: Option<Row> = sqlx::query_as(
            "INSERT INTO server_leases (name, owner, version, acquired_at, renewed_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name) DO UPDATE SET owner = ?2, version = ?3, acquired_at = ?4, renewed_at = ?4
             WHERE server_leases.owner = ?2 OR server_leases.renewed_at < ?5
             RETURNING name, owner, version, acquired_at, renewed_at",
        )
        .bind(name)
        .bind(owner)
        .bind(version)
        .bind(bind_ts(now))
        .bind(bind_ts(before))
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        match taken {
            Some(row) => Ok(Acquired::Taken(lease_row(row)?)),
            None => Ok(Acquired::HeldBy(self.current(name).await?)),
        }
    }

    async fn renew(&self, name: &str, owner: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
        let done = sqlx::query("UPDATE server_leases SET renewed_at = ?3 WHERE name = ?1 AND owner = ?2")
            .bind(name)
            .bind(owner)
            .bind(bind_ts(now))
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(done.rows_affected() == 1)
    }

    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM server_leases WHERE name = ?1 AND owner = ?2")
            .bind(name)
            .bind(owner)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn current(&self, name: &str) -> Result<Option<LeaseRow>, StoreError> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT name, owner, version, acquired_at, renewed_at FROM server_leases WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(lease_row).transpose()
    }
}

pub struct SqliteServerState {
    pool: SqlitePool,
}

impl SqliteServerState {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ServerStateStore for SqliteServerState {
    async fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("SELECT value FROM server_state WHERE name = ?1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)
    }

    async fn set(&self, name: &str, value: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO server_state (name, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET value = ?2, updated_at = ?3",
        )
        .bind(name)
        .bind(value)
        .bind(bind_ts(now))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::sqlite::{connect, migrate};
    use crate::ports::leases::WRITER;

    async fn store() -> (tempfile::TempDir, SqliteLeaseStore, SqlitePool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let pool = connect(&format!("sqlite:{}?mode=rwc", tmp.path().join("l.db").display()))
            .await
            .unwrap();
        let store = SqliteLeaseStore::new(pool.clone());
        store.ensure().await.unwrap();
        (tmp, store, pool)
    }

    const STALE: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn acquire_takes_a_stale_row_atomically() {
        let (_tmp, store, _) = store().await;
        let then = Utc::now() - chrono::Duration::seconds(120);
        store.acquire(WRITER, "dead", "0", then, STALE).await.unwrap();
        let now = Utc::now();
        let (a, b) = tokio::join!(
            store.acquire(WRITER, "a", "1", now, STALE),
            store.acquire(WRITER, "b", "1", now, STALE)
        );
        let outcomes = [a.unwrap(), b.unwrap()];
        let winners: Vec<_> = outcomes
            .iter()
            .filter_map(|o| match o {
                Acquired::Taken(row) => Some(row.owner.clone()),
                Acquired::HeldBy(_) => None,
            })
            .collect();
        assert_eq!(winners.len(), 1, "{outcomes:?}");
        for outcome in &outcomes {
            if let Acquired::HeldBy(holder) = outcome {
                assert_eq!(holder.as_ref().unwrap().owner, winners[0]);
            }
        }
    }

    #[tokio::test]
    async fn acquire_of_a_live_foreign_lease_returns_the_holder() {
        let (_tmp, store, _) = store().await;
        let now = Utc::now();
        store.acquire(WRITER, "a", "1.2.3", now, STALE).await.unwrap();
        let Acquired::HeldBy(Some(holder)) = store.acquire(WRITER, "b", "9", now, STALE).await.unwrap()
        else {
            panic!("a live foreign lease is held");
        };
        assert_eq!((holder.owner.as_str(), holder.version.as_str()), ("a", "1.2.3"));
    }

    #[tokio::test]
    async fn renew_of_a_foreign_lease_changes_nothing() {
        let (_tmp, store, _) = store().await;
        let then = Utc::now() - chrono::Duration::seconds(5);
        store.acquire(WRITER, "a", "1", then, STALE).await.unwrap();
        assert!(!store.renew(WRITER, "b", Utc::now()).await.unwrap());
        let row = store.current(WRITER).await.unwrap().unwrap();
        assert_eq!(row.renewed_at.timestamp(), then.timestamp());
        assert!(store.renew(WRITER, "a", Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn acquire_is_idempotent_for_the_same_owner() {
        let (_tmp, store, _) = store().await;
        let now = Utc::now();
        assert!(matches!(store.acquire(WRITER, "a", "1", now, STALE).await.unwrap(), Acquired::Taken(_)));
        assert!(matches!(store.acquire(WRITER, "a", "1", now, STALE).await.unwrap(), Acquired::Taken(_)));
        store.release(WRITER, "a").await.unwrap();
        assert!(matches!(store.acquire(WRITER, "b", "1", now, STALE).await.unwrap(), Acquired::Taken(_)));
    }

    #[tokio::test]
    async fn state_set_is_an_upsert() {
        let (_tmp, _, pool) = store().await;
        migrate::run_all(&pool).await.unwrap();
        let state = SqliteServerState::new(pool);
        assert_eq!(state.get("last_backup_at").await.unwrap(), None);
        state.set("last_backup_at", "one", Utc::now()).await.unwrap();
        state.set("last_backup_at", "two", Utc::now()).await.unwrap();
        assert_eq!(state.get("last_backup_at").await.unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn the_lease_table_statement_is_the_migrations() {
        let statement = lease_table_statement();
        assert!(statement.starts_with("CREATE TABLE IF NOT EXISTS server_leases"));
        assert!(!statement.contains("server_state"));
    }
}
