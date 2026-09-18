//! `SweepStorage`: the always-on storage sweep, independent of the retention
//! flags. Each step is bounded per pass and none runs at boot.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::ports::clock::Clock;
use crate::storage::StorageBackend;

/// In-flight writer residue older than this is abandoned.
pub const ABANDONED_AFTER: Duration = Duration::from_secs(3600);

const PERIOD: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub abandoned: u64,
}

pub struct SweepStorage {
    storage: Arc<dyn StorageBackend>,
}

impl SweepStorage {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self { storage }
    }

    pub async fn run(&self, now: DateTime<Utc>) -> SweepReport {
        let mut report = SweepReport::default();
        match self.storage.sweep_abandoned(ABANDONED_AFTER, now).await {
            Ok(n) => report.abandoned = n,
            Err(e) => warn!(error = %e, "storage sweep: abandoned writer residue"),
        }
        info!(abandoned = report.abandoned, "storage sweep complete");
        report
    }
}

/// One pass per period, the first after one period: never at boot.
pub async fn start_storage_sweep(sweep: SweepStorage, clock: Arc<dyn Clock>) {
    loop {
        tokio::time::sleep(PERIOD).await;
        sweep.run(clock.now()).await;
    }
}
