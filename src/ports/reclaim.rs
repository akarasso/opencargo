//! Port 22 (A1 C1): deferred reclamation of shared keys. Pins fence a
//! placement, candidates queue what a transaction proved orphaned, claims
//! let one reclaimer delete a key no row references and no pin protects.
//!
//! Expiries only free dead holders: a claim revokes every pin on its key and
//! marks the generation so it is never pinned again, and a commit whose pin
//! is gone answers `Superseded`. No transaction here serves another port;
//! the enqueues of `delete_version` and `retire` are written by those
//! methods' own adapters, in their own transactions.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

/// A placement's right to commit a row referencing `physical_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinToken {
    pub token: String,
    pub logical_key: String,
    pub physical_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pinned {
    Tokens(Vec<PinToken>),
    /// The prefix belongs to a retired incarnation.
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimToken(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Claimed(ClaimToken),
    Referenced,
    Pinned,
    NotDue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renewal {
    Renewed,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: String,
    /// Everything under the key, as a retired incarnation's prefix is.
    pub prefix: bool,
}

#[async_trait]
pub trait ReclaimStore: Send + Sync {
    /// One transaction for every key of one placement. A generation is
    /// reused only when a committed row references it and it was never
    /// claimed; otherwise a fresh opaque one is allocated. Never waits.
    async fn pin(
        &self,
        repo_prefix: &str,
        logical_keys: &[String],
        until: DateTime<Utc>,
    ) -> Result<Pinned, StoreError>;

    /// Idempotent.
    async fn enqueue(&self, physical_keys: &[String], now: DateTime<Utc>) -> Result<(), StoreError>;

    async fn enqueue_prefix(&self, prefix: &str, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Candidates enqueued before `now - grace`, and every prefix of a
    /// retired incarnation whatever its age; none held by a live claim.
    async fn due(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Candidate>, StoreError>;

    /// One transaction: unreferenced, unprotected and due, then every pin on
    /// the key (or under the prefix) is revoked and the generation marked
    /// claimed for good. `Referenced` drops the candidate.
    async fn claim(
        &self,
        key: &str,
        grace: Duration,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Claim, StoreError>;

    /// A compare-and-set on the token.
    async fn renew(
        &self,
        token: &ClaimToken,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Renewal, StoreError>;

    /// Drops the claim and its candidate; a stale token is a no-op.
    async fn release(&self, token: &ClaimToken) -> Result<(), StoreError>;

    /// Forgets the claimed mark of a generation `stat` showed absent.
    async fn forget_claimed(&self, physical_key: &str) -> Result<(), StoreError>;

    /// Drops the retired mark of a prefix whose final listing was empty.
    async fn forget_retired(&self, prefix: &str) -> Result<(), StoreError>;

    /// Deletes pins expired for longer than `grace`, at most `limit`.
    async fn prune_pins(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError>;
}
