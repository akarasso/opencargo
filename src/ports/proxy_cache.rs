//! What the proxy remembers of its upstreams: one row per cached answer,
//! read on every artifact the server serves from a proxy repository.
//!
//! Freshness and retention are the two places a dialect would otherwise live,
//! because both compare a stored column against a clock. Here the clock is
//! always the caller's: `now` is a parameter of every method that writes a
//! timestamp or whose predicate reads one, so no column default fires and no
//! adapter spells its own date arithmetic.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{CacheEntry, CacheEntryId, NewEntry, RepoId};
use crate::error::StoreError;

#[async_trait]
pub trait ProxyCacheStore: Send + Sync {
    /// The row under `kind`/`key`, with [`CacheEntry::fresh`] answered
    /// against `now` — never against the database's clock.
    async fn entry(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheEntry>, StoreError>;

    /// Remember an answer, replacing whatever stood under the same key.
    /// `now` fills `fetched_at` and `last_used_at`, and dates the expiry.
    async fn upsert(&self, entry: &NewEntry<'_>, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Keep a row out of the sweep's reach, and — when the upstream
    /// revalidated it — push its expiry to `now + ttl`. `None` leaves the
    /// expiry alone, which is what a plain hit does.
    async fn touch(
        &self,
        id: CacheEntryId,
        ttl: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Forget everything a repository cached, legacy rows included; returns
    /// how many entries went.
    async fn delete_for_repo(&self, repo: RepoId) -> Result<u64, StoreError>;

    /// What the sweep may evict, oldest first and at most `limit`: expired
    /// negative answers, and anything unused for `idle`. A stale positive row
    /// is not evictable — it still serves while its upstream is down.
    async fn evictable(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CacheEntry>, StoreError>;

    /// Forget one row. Idempotent: the sweep and a purge can race for it.
    async fn delete(&self, id: CacheEntryId) -> Result<(), StoreError>;
}
