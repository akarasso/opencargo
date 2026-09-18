//! The writer lease: taken before the first write, renewed while serving,
//! released on the way out. Two types, because the state every request
//! clones cannot own the renewal task that only one owner may stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::error::StoreError;
use crate::ports::clock::Clock;
use crate::ports::leases::{Acquired, LeaseRow, LeaseStore, WRITER};

/// How long to wait for a holder, when a lease goes stale, and how often a
/// held one is renewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTerms {
    pub wait: Duration,
    pub stale_after: Duration,
    pub renew: Duration,
}

impl LeaseTerms {
    /// Between two attempts while another instance holds the lease.
    const POLL: Duration = Duration::from_secs(1);
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "another instance holds the writer lease: owner {owner}, version {version}, renewed at {renewed_at}; \
         stop it, or wait for its lease to go stale"
    )]
    Held {
        owner: String,
        version: String,
        renewed_at: String,
    },
    #[error("the writer lease could not be read: {0}")]
    Store(#[from] StoreError),
}

impl From<LeaseRow> for LeaseError {
    fn from(row: LeaseRow) -> Self {
        LeaseError::Held {
            owner: row.owner,
            version: row.version,
            renewed_at: row.renewed_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

/// What `held` reports is what the lease is for anyone who asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseStatus {
    Held,
    Lost,
    Disabled,
}

impl LeaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseStatus::Held => "held",
            LeaseStatus::Lost => "lost",
            LeaseStatus::Disabled => "disabled",
        }
    }
}

/// A cheap view of the lease, for the state every request clones and for
/// the background runners that must run only on the holder.
#[derive(Clone)]
pub struct LeaseHandle {
    owner: Arc<str>,
    held: Arc<AtomicBool>,
    disabled: bool,
}

impl LeaseHandle {
    /// No lease is taken: every runner behaves as the holder.
    pub fn disabled() -> Self {
        Self {
            owner: Arc::from("disabled"),
            held: Arc::new(AtomicBool::new(true)),
            disabled: true,
        }
    }

    /// False only once a renewal found the lease taken by someone else.
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn status(&self) -> LeaseStatus {
        match (self.disabled, self.held()) {
            (true, _) => LeaseStatus::Disabled,
            (false, true) => LeaseStatus::Held,
            (false, false) => LeaseStatus::Lost,
        }
    }
}

/// The lease itself: not `Clone`, and `release` consumes it, so exactly one
/// owner can give it back.
///
/// ```compile_fail
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<opencargo::app::lease::LeaseGuard>();
/// ```
pub struct LeaseGuard {
    handle: LeaseHandle,
    store: Arc<dyn LeaseStore>,
    renewals: Option<JoinHandle<()>>,
}

impl LeaseGuard {
    /// Waits up to `terms.wait` for a live holder to release or go stale,
    /// then refuses naming it; on success the renewals are running.
    pub async fn take(
        store: Arc<dyn LeaseStore>,
        clock: Arc<dyn Clock>,
        owner: &str,
        version: &str,
        terms: LeaseTerms,
    ) -> Result<LeaseGuard, LeaseError> {
        let deadline = Instant::now() + terms.wait;
        loop {
            let attempt = store
                .acquire(WRITER, owner, version, clock.now(), terms.stale_after)
                .await;
            let refusal = match attempt {
                Ok(Acquired::Taken(_)) => break,
                Ok(Acquired::HeldBy(Some(holder))) => LeaseError::from(holder),
                Ok(Acquired::HeldBy(None)) => LeaseError::Store(StoreError::Unavailable),
                Err(err) => LeaseError::Store(err),
            };
            let now = Instant::now();
            if now >= deadline {
                return Err(refusal);
            }
            tokio::time::sleep(LeaseTerms::POLL.min(deadline - now)).await;
        }
        info!(owner, "writer lease taken");
        let handle = LeaseHandle {
            owner: Arc::from(owner),
            held: Arc::new(AtomicBool::new(true)),
            disabled: false,
        };
        let renewals = tokio::spawn(renew(
            store.clone(),
            clock,
            handle.clone(),
            version.to_string(),
            terms,
        ));
        Ok(LeaseGuard {
            handle,
            store,
            renewals: Some(renewals),
        })
    }

    pub fn handle(&self) -> LeaseHandle {
        self.handle.clone()
    }

    /// Stops renewing, then gives the lease back so a successor takes it
    /// on its first attempt.
    pub async fn release(mut self) {
        if let Some(renewals) = self.renewals.take() {
            renewals.abort();
            let _ = renewals.await;
        }
        if let Err(err) = self.store.release(WRITER, self.handle.owner()).await {
            warn!(error = %err, "writer lease not released; it goes stale on its own");
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(renewals) = self.renewals.take() {
            renewals.abort();
        }
    }
}

/// A failed renewal is retried and never clears `held`; a renewal that
/// finds the lease foreign clears it once, then keeps trying to win it back.
async fn renew(
    store: Arc<dyn LeaseStore>,
    clock: Arc<dyn Clock>,
    handle: LeaseHandle,
    version: String,
    terms: LeaseTerms,
) {
    let mut every = tokio::time::interval_at(Instant::now() + terms.renew, terms.renew);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let now: DateTime<Utc> = clock.now();
        if handle.held() {
            match store.renew(WRITER, handle.owner(), now).await {
                Ok(true) => {}
                Ok(false) => {
                    handle.held.store(false, Ordering::Release);
                    let holder = store.current(WRITER).await.ok().flatten().map(|r| r.owner);
                    warn!(owner = handle.owner(), holder = ?holder, "writer lease lost to another instance");
                }
                Err(err) => warn!(error = %err, "writer lease renewal failed; retrying"),
            }
        } else if let Ok(Acquired::Taken(_)) = store
            .acquire(WRITER, handle.owner(), &version, now, terms.stale_after)
            .await
        {
            handle.held.store(true, Ordering::Release);
            info!(owner = handle.owner(), "writer lease taken back");
        }
    }
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod tests;
