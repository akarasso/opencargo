//! The explicit grants the permission ladder consults.
//!
//! The port answers what a user was granted on a repository; what that means
//! against their role is [`crate::domain::allows`]'s answer, not a query's.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::Rights;
use crate::error::StoreError;

/// One grant as the admin screen lists it. The repository's name is `None`
/// once the repository is gone — a state the screen has to show rather than
/// hide, and the reason this is not two lookups.
pub struct RepoRights {
    pub repository_id: i64,
    pub repository: Option<String>,
    pub rights: Rights,
}

#[async_trait]
pub trait PermissionStore: Send + Sync {
    /// The grant a user holds on a repository, or `None` when they hold none
    /// — which is not the same as a grant that allows nothing.
    async fn rights(
        &self,
        user_id: i64,
        repository_id: i64,
    ) -> Result<Option<Rights>, StoreError>;

    /// Every grant one user holds, oldest first.
    async fn of_user(&self, user_id: i64) -> Result<Vec<RepoRights>, StoreError>;

    /// Create or replace the grant. `now` fills `created_at` on the create,
    /// so no column default ever fires.
    async fn set(
        &self,
        user_id: i64,
        repository_id: i64,
        rights: Rights,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Idempotent: a grant that is not there is already the state asked for.
    async fn revoke(&self, user_id: i64, repository_id: i64) -> Result<(), StoreError>;
}
