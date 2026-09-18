//! `ReclaimOrphans`: the only deleter of shared keys (A1 C5bis). Over
//! `ReclaimStore`, `ReferencedKeys` and `StorageBackend`, with no backend
//! knowledge: prune dead pins, claim what is due, delete under the claim,
//! release; then a scan that reports what no row references.
//!
//! Safety comes from the claim's fencing, never from these durations: they
//! only bound how long a dead holder blocks the others.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, TryStreamExt};
use tracing::{info, warn};

use crate::app::mark::HighWaterMark;
use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::reclaim::{Candidate, Claim, ClaimToken, ReclaimStore, Renewal};
use crate::ports::referenced::ReferencedKeys;
use crate::storage::{StorageBackend, StorageError};

#[derive(Debug, Clone, Copy)]
pub struct ReclaimPolicy {
    /// How long an enqueued candidate, an unreferenced object or an expired
    /// pin waits before it is acted on.
    pub grace: Duration,
    /// Candidates, pins and scanned objects per pass.
    pub limit: u32,
    /// Whether scan candidates are enqueued, or only reported.
    pub act_on_scan: bool,
}

impl Default for ReclaimPolicy {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(2 * 3600),
            limit: 1000,
            act_on_scan: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReclaimReport {
    pub pruned_pins: u64,
    pub reclaimed: u64,
    pub referenced: u64,
    pub pinned: u64,
    pub failed: u64,
    /// Objects older than the grace that nothing references.
    pub scan_orphans: u64,
    /// The high-water mark refused the batch: nothing was deleted.
    pub refused: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Done {
    Reclaimed,
    Referenced,
    Pinned,
    NotDue,
}

pub struct ReclaimOrphans {
    store: Arc<dyn ReclaimStore>,
    referenced: Arc<dyn ReferencedKeys>,
    storage: Arc<dyn StorageBackend>,
    mark: HighWaterMark,
    policy: ReclaimPolicy,
}

impl ReclaimOrphans {
    pub fn new(
        store: Arc<dyn ReclaimStore>,
        referenced: Arc<dyn ReferencedKeys>,
        storage: Arc<dyn StorageBackend>,
        policy: ReclaimPolicy,
    ) -> Self {
        Self {
            mark: HighWaterMark::new(store.clone(), storage.clone()),
            store,
            referenced,
            storage,
            policy,
        }
    }

    /// Every batch compares the mark first (C-3, C-5) and writes it one
    /// ahead of the counter it then records (C-6). A refusal deletes
    /// nothing and leaves the candidates for the pass after the verify.
    async fn permitted(&self) -> bool {
        match self.mark.permit().await {
            Ok(true) => true,
            Ok(false) => {
                warn!("reclaim: the high-water mark refuses this batch, storage verify is owed");
                false
            }
            Err(e) => {
                warn!(error = %e, "reclaim: the high-water mark could not be read");
                false
            }
        }
    }

    /// One pass, bounded by the policy's limit at every step.
    pub async fn run(&self, now: DateTime<Utc>) -> ReclaimReport {
        let mut report = ReclaimReport::default();
        if !self.permitted().await {
            report.refused = true;
            return report;
        }
        match self.store.prune_pins(self.policy.grace, now, self.policy.limit).await {
            Ok(n) => report.pruned_pins = n,
            Err(e) => warn!(error = %e, "reclaim: pin pruning failed"),
        }
        match self.store.due(self.policy.grace, now, self.policy.limit).await {
            Ok(due) => {
                for candidate in &due {
                    self.tally(&mut report, candidate, now).await;
                }
            }
            Err(e) => warn!(error = %e, "reclaim: reading due candidates failed"),
        }
        match self.scan(now).await {
            Ok(n) => report.scan_orphans = n,
            Err(e) => warn!(error = %e, "reclaim: scan failed"),
        }
        info!(?report, "reclaim pass complete");
        report
    }

    /// The given candidates now, as `RetireRepository` asks for its
    /// prefixes: each claim still re-checks.
    pub async fn now(&self, candidates: &[Candidate], now: DateTime<Utc>) -> ReclaimReport {
        let mut report = ReclaimReport::default();
        if !self.permitted().await {
            report.refused = true;
            return report;
        }
        for candidate in candidates {
            self.tally(&mut report, candidate, now).await;
        }
        report
    }

    async fn tally(&self, report: &mut ReclaimReport, candidate: &Candidate, now: DateTime<Utc>) {
        match self.one(candidate, now).await {
            Ok(Done::Reclaimed) => report.reclaimed += 1,
            Ok(Done::Referenced) => report.referenced += 1,
            Ok(Done::Pinned) => report.pinned += 1,
            Ok(Done::NotDue) => {}
            Err(e) => {
                warn!(key = %candidate.key, error = %e, "reclaim: candidate left for the next pass");
                report.failed += 1;
            }
        }
    }

    fn claim_until(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let bound = self.storage.upload_plan().delete_bound * 2 + Duration::from_secs(60);
        now + chrono::Duration::from_std(bound).unwrap_or(chrono::Duration::hours(1))
    }

    async fn one(&self, candidate: &Candidate, now: DateTime<Utc>) -> Result<Done, Failure> {
        let until = self.claim_until(now);
        let token = match self
            .store
            .claim(&candidate.key, self.policy.grace, now, until)
            .await?
        {
            Claim::Claimed(token) => token,
            Claim::Referenced => return Ok(Done::Referenced),
            Claim::Pinned => return Ok(Done::Pinned),
            Claim::NotDue => return Ok(Done::NotDue),
        };
        if candidate.prefix {
            self.delete_prefix(&candidate.key, &token, now).await?;
        } else {
            self.storage.delete(&candidate.key).await?;
            self.store.release(&token).await?;
            if self.storage.stat(&candidate.key).await?.is_none() {
                self.store.forget_claimed(&candidate.key).await?;
            }
        }
        Ok(Done::Reclaimed)
    }

    /// Bounded batches with the claim renewed between them; a renewal that
    /// lost the claim stops, leaving the rest to its new holder.
    async fn delete_prefix(
        &self,
        prefix: &str,
        token: &ClaimToken,
        now: DateTime<Utc>,
    ) -> Result<(), Failure> {
        let batch = self.storage.upload_plan().delete_batch.max(1);
        loop {
            let keys: Vec<String> = self
                .storage
                .list(prefix)
                .take(batch)
                .map_ok(|meta| meta.key)
                .try_collect()
                .await?;
            if keys.is_empty() {
                break;
            }
            self.storage.delete_batch(&keys).await?;
            if self.store.renew(token, now, self.claim_until(now)).await? == Renewal::Superseded {
                return Ok(());
            }
        }
        self.store.forget_retired(prefix).await?;
        self.store.release(token).await?;
        Ok(())
    }

    /// Every stored object older than the grace that nothing references is
    /// a scan candidate: reported, and enqueued only when the policy acts on
    /// them. Bounded by the policy's limit on objects looked at.
    async fn scan(&self, now: DateTime<Utc>) -> Result<u64, Failure> {
        let referenced: Vec<_> = self
            .referenced
            .referenced(self.policy.grace, now)
            .try_collect()
            .await?;
        let exact: HashSet<&str> = referenced
            .iter()
            .filter(|r| !r.prefix)
            .map(|r| r.key.as_str())
            .collect();
        let prefixes: Vec<&str> = referenced
            .iter()
            .filter(|r| r.prefix)
            .map(|r| r.key.as_str())
            .collect();
        let cutoff = now - chrono::Duration::from_std(self.policy.grace).unwrap_or_default();
        let budget = self.policy.limit as usize * 100;
        let mut objects = self.storage.list("").take(budget);
        let mut orphans = Vec::new();
        while let Some(meta) = objects.next().await {
            let meta = meta?;
            if layout::reserved(&meta.key) {
                continue;
            }
            let covered = exact.contains(meta.key.as_str())
                || prefixes.iter().any(|p| layout::under(&meta.key, p));
            if !covered && meta.last_modified < cutoff {
                orphans.push(meta.key);
            }
        }
        if self.policy.act_on_scan && !orphans.is_empty() {
            self.store.enqueue(&orphans, now - chrono::Duration::from_std(self.policy.grace).unwrap_or_default()).await?;
        }
        Ok(orphans.len() as u64)
    }
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[cfg(test)]
#[path = "reclaim_tests.rs"]
mod tests;
