//! The multipart ledger: which server-side uploads are open right now.
//!
//! A multipart upload lives in the object store, not in this process, so it
//! outlives the writer that created it: a dropped writer aborts its own upload,
//! but a killed process leaves parts behind that nothing would ever bill down.
//! The ledger is the durable trace a sweep reads. It is a port rather than a
//! table so the storage backend never holds a pool, and so a deployment whose
//! store is not SQLite answers with its own.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

/// An upload the sweep may abort: its id, and the key it was writing.
pub type Abandoned = (String, String);

#[async_trait]
pub trait MultipartLedger: Send + Sync {
    /// Record an upload the backend has just created.
    async fn opened(
        &self,
        upload_id: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Keep an upload out of [`MultipartLedger::idle_since`]'s reach. A writer
    /// calls this at most once per quarter of the sweep's age, never per part:
    /// a 4 GiB layer is 512 parts, and the value is only ever read against an
    /// hour-wide threshold.
    ///
    /// An upload the sweep has already collected is not an error here — the
    /// next part is what fails, with the store's answer, not this bump.
    async fn touched(&self, upload_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Forget an upload that completed or aborted. Idempotent: a writer's
    /// `Drop` and a sweep can race for the same row.
    async fn closed(&self, upload_id: &str) -> Result<(), StoreError>;

    /// Every upload untouched for `age`, oldest first — what the sweep aborts.
    ///
    /// `now` is the caller's, never the store's clock: the comparison is the
    /// one place a dialect would otherwise reappear.
    async fn idle_since(
        &self,
        age: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<Abandoned>, StoreError>;

    /// How many uploads are open, for the storage status tile.
    /// [`MultipartLedger::idle_since`] cannot answer it: it is the complement,
    /// and its threshold hides exactly the healthy ones.
    async fn in_flight(&self) -> Result<u64, StoreError>;
}
