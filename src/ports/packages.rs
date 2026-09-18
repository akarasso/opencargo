//! Packages, their versions and their dist-tags: the publish and resolve hot
//! path of every format.
//!
//! Three methods are coarse, and all for the same reason — they write more
//! than one row, and a half-written publish or a half-deleted version is
//! visible to the next reader. Everything else is a single statement and
//! needs no boundary.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{DistTag, Package, Version};
use crate::ports::reclaim::PinToken;
use crate::error::StoreError;

/// How a package name is matched. Cargo's names are unique regardless of
/// case and its clients send whatever case they like; npm's and Go's are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameMatch {
    Exact,
    Insensitive,
}

/// One version to publish, with every row the transaction writes.
///
/// The package row is upserted inside it: a publish that created the package
/// and then failed would leave an empty package the search index has already
/// picked up.
pub struct NewRelease<'a> {
    pub repository: i64,
    pub package: &'a str,
    pub match_name: NameMatch,
    /// Set on the package row only when it is created.
    pub description: Option<&'a str>,
    /// Replaces the stored README when present; `None` leaves it alone, which
    /// is what a metadata-only publish needs.
    pub readme: Option<&'a str>,
    pub version: &'a str,
    pub metadata_json: &'a str,
    pub checksum_sha1: Option<&'a str>,
    pub checksum_sha256: Option<&'a str>,
    pub integrity: Option<&'a str>,
    pub size: i64,
    pub tarball_path: &'a str,
    /// The tags that point at this version once it exists.
    pub dist_tags: &'a [String],
    /// Edges out of this version, written in the same transaction so a
    /// visible version never lacks them.
    pub dependencies: &'a [ReleaseDependency<'a>],
    /// The pins of the keys the row references, spent by compare-and-set in
    /// the transaction; a revoked one makes the publish write nothing.
    pub pins: &'a [PinToken],
    pub now: DateTime<Utc>,
}

/// One dependency edge of a release, recorded with its version row.
pub struct ReleaseDependency<'a> {
    pub name: &'a str,
    pub requirement: &'a str,
    pub kind: &'a str,
}

/// What a publish landed: both rows, as stored.
#[derive(Debug)]
pub struct Release {
    pub package: Package,
    pub version: Version,
}

/// The audit row a promotion writes inside its transaction, so a promoted
/// version and the record of who promoted it cannot disagree.
pub struct PromotionAudit<'a> {
    /// `None` for a static token, which has no user row behind it.
    pub user_id: Option<i64>,
    pub username: &'a str,
    pub target: &'a str,
    pub repository: &'a str,
    pub details_json: &'a str,
}

/// A version already published elsewhere, arriving in another repository.
///
/// The blob is placed *before* this runs: a gigabyte-long copy inside the
/// transaction would hold SQLite's single writer for its whole duration.
pub struct Promotion<'a> {
    pub source: &'a Version,
    pub target_repository: i64,
    pub package: &'a str,
    pub description: Option<&'a str>,
    pub metadata_json: &'a str,
    pub tarball_path: &'a str,
    /// The tags the source version holds, which the promoted one inherits.
    pub dist_tags: &'a [String],
    /// As for a publish: spent in the transaction, all or none.
    pub pins: &'a [PinToken],
    pub audit: PromotionAudit<'a>,
    pub now: DateTime<Utc>,
}

/// A pre-release version past its retention, with the two names its log
/// line carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalePrerelease {
    /// The version row, which is what [`PackageStore::delete_version`] takes.
    pub id: i64,
    pub package: String,
    pub version: String,
    pub tarball_path: String,
}

#[async_trait]
pub trait PackageStore: Send + Sync {
    async fn package(
        &self,
        repository: i64,
        name: &str,
        how: NameMatch,
    ) -> Result<Option<Package>, StoreError>;

    /// The first package of that name in any repository, which is how the
    /// dependency graph and the vulnerability screens address one: they are
    /// given a bare name, with no repository in the path. `public_only`
    /// restricts the search to publicly-visible repositories.
    async fn anywhere(
        &self,
        name: &str,
        public_only: bool,
    ) -> Result<Option<Package>, StoreError>;

    /// In publication order, which is the order every format's index serves.
    async fn versions(&self, package: i64) -> Result<Vec<Version>, StoreError>;

    async fn version(&self, package: i64, version: &str)
        -> Result<Option<Version>, StoreError>;

    /// The package's version stamp (A1 C5), opaque: every publish, yank,
    /// unyank and delete of one of its versions moves it, nothing else does,
    /// and equal stamps mean the same version rows in the same listing
    /// state, so a reader validates what it derived from them without
    /// reading them again.
    async fn stamp(&self, package: i64) -> Result<String, StoreError>;

    async fn dist_tags(&self, package: i64) -> Result<Vec<DistTag>, StoreError>;

    /// The package upsert, the version row and this version's dist-tags, in
    /// one transaction.
    ///
    /// `Conflict` when that `package@version` is already there, including
    /// when two callers race for it: the unique constraint is the arbiter,
    /// not a preceding read. `Superseded` when a pin was revoked.
    async fn publish_version(&self, release: &NewRelease<'_>) -> Result<Release, StoreError>;

    /// The package upsert in the target repository, the version row, its
    /// inherited dist-tags and the audit row, in one transaction.
    ///
    /// `Conflict` when the target repository already holds that version.
    async fn promote_metadata(&self, promotion: &Promotion<'_>) -> Result<Version, StoreError>;

    async fn set_dist_tag(
        &self,
        package: i64,
        tag: &str,
        version: i64,
    ) -> Result<(), StoreError>;

    /// `NotFound` if the package never carried that tag.
    async fn clear_dist_tag(&self, package: i64, tag: &str) -> Result<(), StoreError>;

    /// Raw markdown; the caller caps and the renderer sanitizes it.
    async fn set_readme(
        &self,
        package: i64,
        readme: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Replace a version's stored metadata, which is what `npm deprecate` is.
    async fn set_metadata(&self, version: i64, metadata_json: &str) -> Result<(), StoreError>;

    async fn set_yanked(&self, version: i64, yanked: bool) -> Result<(), StoreError>;

    /// The pre-releases published more than `older_than` before `now`, in
    /// publication order — and only in npm and cargo repositories, because a Go
    /// pseudo-version and an OCI tag carry a `-` while being permanent
    /// artifacts that a retention sweep must never touch.
    ///
    /// `now` is the caller's, so the predicate never reads the store's clock.
    async fn stale_prereleases(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<StalePrerelease>, StoreError>;

    /// The version row and everything hanging off it — its dist-tags, its
    /// download rows and its counter — in one transaction; `NotFound` once
    /// it is gone. None of those four foreign keys cascades, so the deletion
    /// is this method's to perform and not the schema's.
    ///
    /// The artifact's key is enqueued for reclamation in the same
    /// transaction; nothing is deleted from storage (A1 C5bis N1).
    async fn delete_version(&self, version: i64, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// One more download of a version, as a counter rather than a row per
    /// download. Best-effort at every call site: a served artifact is not
    /// unserved because the tally could not be kept.
    async fn record_download(&self, version: i64) -> Result<(), StoreError>;
}
