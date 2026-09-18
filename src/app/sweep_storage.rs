//! `SweepStorage`: the always-on storage sweep, independent of the retention
//! flags. Each step is bounded per pass and none runs at boot.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::app::reclaim::{ReclaimOrphans, ReclaimReport};
use crate::ports::clock::Clock;
use crate::ports::oci::OciStore;
use crate::storage::StorageBackend;

/// In-flight writer residue older than this is abandoned.
pub const ABANDONED_AFTER: Duration = Duration::from_secs(3600);

const PERIOD: Duration = Duration::from_secs(3600);

/// An upload session idle this long is abandoned by its client.
pub const UPLOAD_IDLE: Duration = Duration::from_secs(24 * 3600);

const UPLOADS_PER_PASS: u32 = 1000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub abandoned: u64,
    pub uploads: u64,
    pub reclaim: Option<ReclaimReport>,
}

pub struct SweepStorage {
    storage: Arc<dyn StorageBackend>,
    reclaim: Option<ReclaimOrphans>,
    uploads: Option<Arc<dyn OciStore>>,
}

impl SweepStorage {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            reclaim: None,
            uploads: None,
        }
    }

    /// Stale and legacy upload sessions: rows removed, prefixes enqueued.
    pub fn reaping_uploads(mut self, oci: Arc<dyn OciStore>) -> Self {
        self.uploads = Some(oci);
        self
    }

    pub fn reclaiming(mut self, reclaim: ReclaimOrphans) -> Self {
        self.reclaim = Some(reclaim);
        self
    }

    pub async fn run(&self, now: DateTime<Utc>) -> SweepReport {
        let mut report = SweepReport::default();
        match self.storage.sweep_abandoned(ABANDONED_AFTER, now).await {
            Ok(n) => report.abandoned = n,
            Err(e) => warn!(error = %e, "storage sweep: abandoned writer residue"),
        }
        if let Some(oci) = &self.uploads {
            match oci.reap_uploads(UPLOAD_IDLE, now, UPLOADS_PER_PASS).await {
                Ok(n) => report.uploads = n,
                Err(e) => warn!(error = %e, "storage sweep: abandoned upload sessions"),
            }
        }
        if let Some(reclaim) = &self.reclaim {
            report.reclaim = Some(reclaim.run(now).await);
        }
        info!(abandoned = report.abandoned, "storage sweep complete");
        report
    }
}

/// One pass per period, the first after one period: never at boot.
pub async fn start_storage_sweep(
    sweep: SweepStorage,
    clock: Arc<dyn Clock>,
    lease: crate::app::lease::LeaseHandle,
) {
    loop {
        tokio::time::sleep(PERIOD).await;
        if lease.held() {
            sweep.run(clock.now()).await;
        }
    }
}
