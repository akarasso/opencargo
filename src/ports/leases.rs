//! The writer lease and the server's own small records: which process is
//! the one instance, and what it last did (a backup, a sweep).
//!
//! A guard, not fencing: nothing on a request path consults it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

/// The one lease an instance takes before it writes anything.
pub const WRITER: &str = "writer";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub name: String,
    pub owner: String,
    pub version: String,
    pub acquired_at: DateTime<Utc>,
    pub renewed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    Taken(LeaseRow),
    /// A live foreign holder; `None` when it vanished between the attempt
    /// and the read of its identity, which is a retry, not a refusal.
    HeldBy(Option<LeaseRow>),
}

#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Creates the lease table alone, so a lease can be taken before any
    /// migration runs.
    async fn ensure(&self) -> Result<(), StoreError>;

    /// Takes `name` when it is free, stale past `stale_after`, or already
    /// `owner`'s; one statement, atomic under the single writer.
    async fn acquire(
        &self,
        name: &str,
        owner: &str,
        version: &str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> Result<Acquired, StoreError>;

    /// `Ok(false)`: the lease is someone else's now. An error is not a loss.
    async fn renew(&self, name: &str, owner: &str, now: DateTime<Utc>) -> Result<bool, StoreError>;

    async fn release(&self, name: &str, owner: &str) -> Result<(), StoreError>;

    async fn current(&self, name: &str) -> Result<Option<LeaseRow>, StoreError>;
}

/// Named values the server records about itself.
#[async_trait]
pub trait ServerStateStore: Send + Sync {
    async fn get(&self, name: &str) -> Result<Option<String>, StoreError>;

    async fn set(&self, name: &str, value: &str, now: DateTime<Utc>) -> Result<(), StoreError>;
}
