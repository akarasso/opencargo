//! Vulnerability scanning, split in two: the feed that knows what is wrong
//! with a dependency, and the store that remembers what a scan concluded.
//!
//! They are two ports because they are two failure modes and two second
//! implementations: the feed is an HTTP round trip to OSV, the store is rows.
//! Keeping them apart is also what lets the round trip happen *outside* the
//! write — a store method touches nothing but the database.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{ScanResult, VulnDetail};
use crate::error::StoreError;

/// How a scan fails. A feed produces `Upstream` or `Unscannable`; `Store` is
/// here so the use case that does both has one vocabulary.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("OSV query failed: {0}")]
    Upstream(String),

    /// The document cannot say what the version depends on, so no result
    /// exists to record: a clean row would claim a set that was never seen.
    #[error("dependencies unknown: {0}")]
    Unscannable(String),

    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a scan of one version concluded, as it was recorded.
pub struct VulnScan {
    pub scanned_at: DateTime<Utc>,
    pub total_deps: i64,
    pub vulnerable_deps: i64,
    pub status: String,
    /// The findings of the stored document, or `None` when it holds none it
    /// recognises: rows written before the current shape keep theirs, and a
    /// scan that cannot be re-read is still a scan that happened.
    pub details: Option<serde_json::Value>,
}

/// What an advisory feed can answer. `enabled` is not a configuration read:
/// a disabled feed reports nothing found, which must not be mistaken for a
/// version that was looked at and came back clean.
#[async_trait]
pub trait VulnFeed: Send + Sync {
    fn enabled(&self) -> bool;

    /// The advisories against each `(name, version)`, one entry per input in
    /// the order given.
    async fn assess_batch(
        &self,
        ecosystem: &str,
        deps: &[(String, String)],
    ) -> Result<Vec<Vec<VulnDetail>>, ScanError>;

    /// The same question asked of a version's own metadata document, already
    /// summarised. A disabled feed answers [`ScanResult::clean`].
    async fn assess(
        &self,
        metadata_json: &str,
        ecosystem: &str,
    ) -> Result<ScanResult, ScanError>;
}

#[async_trait]
pub trait VulnStore: Send + Sync {
    /// The newest scan of a version, or `None` when it was never scanned —
    /// which is not the same as a scan that found nothing.
    async fn latest(&self, version: i64) -> Result<Option<VulnScan>, StoreError>;

    /// `now` fills `scanned_at`, so no column default ever fires.
    async fn record(
        &self,
        version: i64,
        result: &ScanResult,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Idempotent: a version with no scans is already the state asked for.
    async fn forget(&self, version: i64) -> Result<(), StoreError>;
}
