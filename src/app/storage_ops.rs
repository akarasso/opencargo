//! The operator's storage use cases behind `opencargo storage`:
//! `VerifyStorage` compares what rows reference with what the store holds,
//! `MigrateStorage` copies one store into another, and `ReclaimPrefix`
//! empties a prefix no repository row names any more. None of them knows
//! which backend it runs on.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::app::mark::{HighWaterMark, MarkError};
use crate::app::reclaim::{ReclaimOrphans, ReclaimReport};
use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::reclaim::{Candidate, ReclaimStore};
use crate::ports::referenced::ReferencedKeys;
use crate::storage::{StorageBackend, StorageError, Versioning};

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(
        "--repair needs a store that keeps noncurrent versions: this one keeps none, \
         so a referenced key with no object is a loss to report, not one to undo"
    )]
    NoVersions,

    #[error(transparent)]
    Mark(#[from] MarkError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// What one `storage verify` does beyond listing what rows reference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Verify {
    /// List the objects nothing references.
    pub orphans: bool,
    /// Queue those of them that lie under a live incarnation (I7).
    pub enqueue: bool,
    /// Put the last noncurrent version of a referenced key with no object
    /// back (C-7); refused on a store that keeps none.
    pub repair: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Objects the store holds.
    pub objects: u64,
    /// Keys a committed row references with no object behind them.
    pub missing: Vec<String>,
    /// Objects older than the grace that nothing references; only filled
    /// when asked for.
    pub orphans: Vec<String>,
    /// Of those, the ones queued for reclamation.
    pub enqueued: Vec<String>,
    /// Keys a noncurrent version was put back for.
    pub repaired: Vec<String>,
}

pub struct VerifyStorage {
    referenced: Arc<dyn ReferencedKeys>,
    store: Arc<dyn ReclaimStore>,
    storage: Arc<dyn StorageBackend>,
    grace: Duration,
}

impl VerifyStorage {
    pub fn new(
        referenced: Arc<dyn ReferencedKeys>,
        store: Arc<dyn ReclaimStore>,
        storage: Arc<dyn StorageBackend>,
        grace: Duration,
    ) -> Self {
        Self {
            referenced,
            store,
            storage,
            grace,
        }
    }

    pub async fn run(&self, options: Verify, now: DateTime<Utc>) -> Result<VerifyReport, OpsError> {
        if options.repair && self.storage.versioning().await? != Versioning::Kept {
            return Err(OpsError::NoVersions);
        }
        let orphans = options.orphans || options.enqueue;
        let referenced: Vec<_> = self.referenced.referenced(self.grace, now).try_collect().await?;
        let prefixes: Vec<&str> = referenced
            .iter()
            .filter(|r| r.prefix)
            .map(|r| r.key.as_str())
            .collect();
        let exact: HashSet<&str> = referenced
            .iter()
            .filter(|r| !r.prefix)
            .map(|r| r.key.as_str())
            .collect();
        let cutoff = now - chrono::Duration::from_std(self.grace).unwrap_or_default();
        let mut report = VerifyReport::default();
        let mut present = HashSet::new();
        let mut listed = self.storage.list("");
        while let Some(meta) = listed.next().await {
            let meta = meta?;
            if layout::reserved(&meta.key) {
                continue;
            }
            report.objects += 1;
            let covered = exact.contains(meta.key.as_str())
                || prefixes.iter().any(|p| layout::under(&meta.key, p));
            if orphans && !covered && meta.last_modified < cutoff {
                report.orphans.push(meta.key.clone());
            }
            present.insert(meta.key);
        }
        report.missing = exact
            .into_iter()
            .filter(|k| !present.contains(*k))
            .map(str::to_string)
            .collect();
        report.missing.sort();
        report.orphans.sort();
        if options.repair {
            self.repair(&mut report).await?;
        }
        if options.enqueue {
            self.enqueue(&mut report, now).await?;
        }
        Ok(report)
    }

    /// Every referenced key with no object gets its last noncurrent version
    /// back; what the store has none for stays reported as missing.
    async fn repair(&self, report: &mut VerifyReport) -> Result<(), OpsError> {
        let mut missing = Vec::new();
        for key in std::mem::take(&mut report.missing) {
            if self.storage.restore_last_version(&key).await? {
                report.repaired.push(key);
            } else {
                missing.push(key);
            }
        }
        report.missing = missing;
        Ok(())
    }

    /// What a rollback left behind: an object no row references, under a
    /// live incarnation, goes to the reclamation queue, which re-checks
    /// before it deletes anything.
    async fn enqueue(&self, report: &mut VerifyReport, now: DateTime<Utc>) -> Result<(), OpsError> {
        let live = self.store.live_prefixes().await?;
        report.enqueued = report
            .orphans
            .iter()
            .filter(|key| live.iter().any(|prefix| layout::under(key, prefix)))
            .cloned()
            .collect();
        if !report.enqueued.is_empty() {
            self.store.enqueue(&report.enqueued, now).await?;
        }
        Ok(())
    }
}

/// The epoch settled against the artifact store (C-3, I7): the mark is
/// compared both ways, and the verify it owes runs and lifts the refusal it
/// set. Nothing here deletes: what it finds goes to the queue, which claims
/// before it acts.
pub struct SettleEpoch {
    store: Arc<dyn ReclaimStore>,
    mark: HighWaterMark,
    verify: VerifyStorage,
}

impl SettleEpoch {
    pub fn new(
        store: Arc<dyn ReclaimStore>,
        referenced: Arc<dyn ReferencedKeys>,
        storage: Arc<dyn StorageBackend>,
        grace: Duration,
    ) -> Self {
        Self {
            mark: HighWaterMark::new(store.clone(), storage.clone()),
            verify: VerifyStorage::new(referenced, store.clone(), storage, grace),
            store,
        }
    }

    /// `None` when nothing was owed.
    pub async fn run(&self, now: DateTime<Utc>) -> Result<Option<VerifyReport>, OpsError> {
        self.mark.guard().await?;
        let state = self.store.epoch().await?;
        if !state.verify_pending {
            return Ok(None);
        }
        let report = self
            .verify
            .run(
                Verify {
                    orphans: true,
                    enqueue: true,
                    repair: false,
                },
                now,
            )
            .await?;
        self.store.verified(&state.epoch).await?;
        Ok(Some(report))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrateReport {
    pub copied: u64,
    pub skipped: u64,
    pub bytes: u64,
}

/// Copies every object of `source` into `target` under the same key. A
/// target object is skipped only when its key names its content's sha256
/// and the target's bytes hash to it; an undigested key is always copied
/// again, because an equal size proves nothing.
pub struct MigrateStorage {
    source: Arc<dyn StorageBackend>,
    target: Arc<dyn StorageBackend>,
}

impl MigrateStorage {
    pub fn new(source: Arc<dyn StorageBackend>, target: Arc<dyn StorageBackend>) -> Self {
        Self { source, target }
    }

    pub async fn run(&self, dry_run: bool) -> Result<MigrateReport, OpsError> {
        let mut report = MigrateReport::default();
        let mut listed = self.source.list("");
        while let Some(meta) = listed.next().await {
            let meta = meta?;
            if self.already_there(&meta.key).await? {
                report.skipped += 1;
                continue;
            }
            report.copied += 1;
            report.bytes += meta.size;
            if !dry_run {
                self.copy(&meta.key).await?;
            }
        }
        Ok(report)
    }

    async fn already_there(&self, key: &str) -> Result<bool, OpsError> {
        let Some(digest) = layout::content_digest(key) else {
            return Ok(false);
        };
        if self.target.stat(key).await?.is_none() {
            return Ok(false);
        }
        Ok(sha256_of(self.target.as_ref(), key).await? == digest)
    }

    async fn copy(&self, key: &str) -> Result<(), OpsError> {
        let mut read = self.source.read_stream(key).await?;
        let mut writer = self.target.writer(key).await?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = read
                .body
                .read(&mut buf)
                .await
                .map_err(|_| StorageError::Unavailable)?;
            if n == 0 {
                break;
            }
            writer.reserve(n).await?;
            writer.write(bytes::Bytes::copy_from_slice(&buf[..n])).await?;
        }
        let written = writer.commit().await?;
        if written != read.total {
            return Err(StorageError::Unavailable.into());
        }
        Ok(())
    }
}

async fn sha256_of(storage: &dyn StorageBackend, key: &str) -> Result<String, OpsError> {
    let mut read = storage.read_stream(key).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = read
            .body
            .read(&mut buf)
            .await
            .map_err(|_| StorageError::Unavailable)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Empties `prefix` now, through the claim that re-checks no row
/// references anything under it: what `storage reclaim --prefix` does for a
/// prefix no repository row names.
pub struct ReclaimPrefix {
    store: Arc<dyn ReclaimStore>,
    reclaim: ReclaimOrphans,
    grace: Duration,
}

impl ReclaimPrefix {
    pub fn new(store: Arc<dyn ReclaimStore>, reclaim: ReclaimOrphans, grace: Duration) -> Self {
        Self {
            store,
            reclaim,
            grace,
        }
    }

    pub async fn run(&self, prefix: &str, now: DateTime<Utc>) -> Result<ReclaimReport, OpsError> {
        let due = now - chrono::Duration::from_std(self.grace).unwrap_or_default() - chrono::Duration::seconds(1);
        self.store.enqueue_prefix(prefix, due).await?;
        let candidate = Candidate {
            key: prefix.to_string(),
            prefix: true,
        };
        Ok(self.reclaim.now(&[candidate], now).await)
    }
}

#[cfg(test)]
#[path = "storage_ops_tests.rs"]
mod tests;
