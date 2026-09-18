//! The audit trail: append-only, and read back by the admin screens.
//!
//! Nothing here is coarse. The one audit row that must land with something
//! else is a promotion's, and that one is written inside
//! [`crate::ports::packages::PackageStore::promote_metadata`]'s transaction,
//! where the version it records is written.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

/// One entry to append: borrowed in, owned out.
pub struct NewAuditEntry<'a> {
    /// `None` for a static token, which has no user row behind it.
    pub user_id: Option<i64>,
    pub username: Option<&'a str>,
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub repository: Option<&'a str>,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub details_json: Option<&'a str>,
}

/// One entry as it was recorded. `user_id` outlives the user it names: the
/// trail is meant to survive the account, so nothing cascades onto it.
pub struct AuditEntry {
    pub id: i64,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub repository: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub details_json: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait AuditStore: Send + Sync {
    /// `now` fills `created_at`, so no column default ever fires and the row
    /// carries the caller's clock.
    async fn append(
        &self,
        entry: &NewAuditEntry<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Newest first, one page at a time: the admin audit screen.
    async fn recent(&self, page: i64, size: i64) -> Result<Vec<AuditEntry>, StoreError>;

    /// Newest first: what the trail says was done to one target under one
    /// action, which is how a package's promotions are listed.
    async fn of_target(
        &self,
        action: &str,
        target: &str,
    ) -> Result<Vec<AuditEntry>, StoreError>;
}
