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

use crate::app::reclaim::{ReclaimOrphans, ReclaimReport};
use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::reclaim::{Candidate, ReclaimStore};
use crate::ports::referenced::ReferencedKeys;
use crate::storage::{StorageBackend, StorageError};

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Storage(#[from] StorageError),
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
}

pub struct VerifyStorage {
    referenced: Arc<dyn ReferencedKeys>,
    storage: Arc<dyn StorageBackend>,
    grace: Duration,
}

impl VerifyStorage {
    pub fn new(referenced: Arc<dyn ReferencedKeys>, storage: Arc<dyn StorageBackend>, grace: Duration) -> Self {
        Self {
            referenced,
            storage,
            grace,
        }
    }

    pub async fn run(&self, orphans: bool, now: DateTime<Utc>) -> Result<VerifyReport, OpsError> {
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
        Ok(report)
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
