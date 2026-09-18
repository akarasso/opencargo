//! Port 20 (A1 C1): single-use login handoffs. Consumption is a
//! compare-and-consume: whatever the verdict, a handoff that was found is
//! consumed by the call that found it, so it is used at most once. Expiry only
//! purges handoffs nobody claimed.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

pub struct NewHandoff<'a> {
    pub code_hash: &'a str,
    /// The fingerprint of the attempt's cookie it is bound to.
    pub binding: &'a str,
    pub payload: &'a str,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Consumption {
    Consumed(String),
    AlreadyConsumed,
    Unknown,
    Expired,
    BindingMismatch,
}

#[async_trait]
pub trait LoginHandoffStore: Send + Sync {
    async fn deposit(&self, handoff: &NewHandoff<'_>) -> Result<(), StoreError>;

    async fn consume(
        &self,
        code_hash: &str,
        binding: &str,
        now: DateTime<Utc>,
    ) -> Result<Consumption, StoreError>;

    /// The payload of a live, unconsumed handoff bound to `binding`, without
    /// consuming it: what a confirmation screen shows.
    async fn peek(
        &self,
        code_hash: &str,
        binding: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError>;

    /// Removes handoffs expired before `now`; returns how many.
    async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, StoreError>;
}
