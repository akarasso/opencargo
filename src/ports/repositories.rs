//! The repositories an operator declares: the unit every protocol request is
//! addressed to, and the row the resolver, the permission matrix and the
//! proxy cache all hang off.
//!
//! Two methods are coarse. `create` allocates the repository's opaque
//! incarnation, the owner of every new key it writes. `retire` refuses
//! while packages or group memberships remain, then removes the row, its
//! grants and its cache rows, marks the incarnation retired and enqueues its
//! prefixes for reclamation, together: a repository whose grants outlived it
//! would hand the next repository of that name someone else's readers.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{RepoConfig, RepoSpec, Repository, Visibility};
use crate::error::StoreError;

/// Which fields an update touches; `None` leaves one alone.
#[derive(Default)]
pub struct RepoPatch<'a> {
    pub visibility: Option<Visibility>,
    pub upstream: Option<&'a str>,
    pub config: Option<&'a RepoConfig>,
}

impl RepoPatch<'_> {
    pub fn touches_nothing(&self) -> bool {
        self.visibility.is_none() && self.upstream.is_none() && self.config.is_none()
    }
}

#[async_trait]
pub trait RepositoryStore: Send + Sync {
    /// Case-sensitive: `npm-prod` and `NPM-PROD` are two repositories, and
    /// the name is a storage segment.
    async fn by_name(&self, name: &str) -> Result<Option<Repository>, StoreError>;

    /// The repository a package belongs to, by the id the package carries.
    async fn by_id(&self, id: i64) -> Result<Option<Repository>, StoreError>;

    /// Every repository, by name: the admin list, and the group-membership
    /// scan a delete runs before it refuses.
    async fn all(&self) -> Result<Vec<Repository>, StoreError>;

    /// Every stored name, in name order. Separate from [`Self::all`] because
    /// the startup guard reports the names a schema no longer allows, and a
    /// row it cannot decode must not stand in the way of that report.
    async fn names(&self) -> Result<Vec<String>, StoreError>;

    /// `Conflict` if the name is taken. `now` fills `created_at` and
    /// `updated_at`, so no column default ever fires.
    async fn create(
        &self,
        spec: &RepoSpec<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError>;

    /// `NotFound` if no such repository; a patch that touches nothing reads
    /// the row back without stamping it.
    async fn update(
        &self,
        name: &str,
        patch: &RepoPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError>;

    /// The row, its grants and its proxy-cache rows, the incarnation's
    /// retired mark, the revocation of every pin under its prefixes and the
    /// enqueue of those prefixes, in one transaction. Answers the prefixes
    /// enqueued (none for a group). The name is free from this commit.
    ///
    /// `Conflict` while any package remains or a group lists it, both
    /// re-checked inside the transaction. Nothing is deleted from storage.
    async fn retire(&self, name: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError>;

    /// The opaque incarnation every new key of the repository lies under.
    async fn incarnation(&self, repository: i64) -> Result<Option<String>, StoreError>;

    /// Insert the configured repositories, skipping the names already there:
    /// the config file seeds a deployment, it does not own it afterwards.
    async fn ensure_seeded(
        &self,
        specs: &[RepoSpec<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;
}
