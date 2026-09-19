//! The API tokens that stand for an account on every request.
//!
//! Expiry is not a predicate here. The store hands back the row and
//! [`ApiToken::is_live`] decides against the caller's clock, so "expired"
//! means the same thing to every adapter and needs no dialect to express.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{ApiToken, TokenScope};
use crate::error::StoreError;

/// A token to record. The raw value never reaches the store — only its hash,
/// and the prefix a lookup is keyed on.
pub struct NewToken<'a> {
    pub id: &'a str,
    pub user_id: i64,
    pub name: &'a str,
    pub prefix: &'a str,
    pub token_hash: &'a str,
    pub expires_at: Option<DateTime<Utc>>,
    /// Chosen once, at issue: there is no method to change it, so changing a
    /// scope means revoking the credential and issuing another.
    pub scope: &'a TokenScope,
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    /// The token a presented credential could be, if any: the hash still has
    /// to be verified against it.
    async fn by_prefix(&self, prefix: &str) -> Result<Option<ApiToken>, StoreError>;

    async fn by_id(&self, id: &str) -> Result<Option<ApiToken>, StoreError>;

    /// One account's tokens, newest first.
    async fn of_user(&self, user_id: i64) -> Result<Vec<ApiToken>, StoreError>;

    /// `now` fills `created_at`, so no column default ever fires.
    async fn create(&self, token: &NewToken<'_>, now: DateTime<Utc>)
        -> Result<ApiToken, StoreError>;

    /// `NotFound` if it was already gone.
    async fn delete(&self, id: &str) -> Result<(), StoreError>;

    /// Record that the token authenticated a request at `now`. Best-effort at
    /// the call site: failing to note a use must never fail the request.
    async fn touch(&self, id: &str, now: DateTime<Utc>) -> Result<(), StoreError>;
}
