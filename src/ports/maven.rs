//! Port 18 (A1 C1): Maven's files, grouped by the unit they become visible
//! with — a release version, or one timestamped build of a snapshot.
//!
//! A unit is an aggregate: its files, their computed digests, the checksums
//! clients declared for them, who deposited it first, whether another
//! principal contested it, and whether it is visible. Every change is a
//! compare-and-set on the unit's revision, so the rules deciding a change
//! live in the use case and the adapter only keeps them atomic. A change
//! that references a key spends its pins in the same transaction; one that
//! releases a key enqueues it there and returns it. Nothing here touches
//! storage.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;
use crate::ports::reclaim::PinToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SumAlgorithm {
    Sha1,
    Md5,
    Sha256,
    Sha512,
}

impl SumAlgorithm {
    pub const ALL: [SumAlgorithm; 4] = [
        SumAlgorithm::Sha1,
        SumAlgorithm::Md5,
        SumAlgorithm::Sha256,
        SumAlgorithm::Sha512,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            SumAlgorithm::Sha1 => "sha1",
            SumAlgorithm::Md5 => "md5",
            SumAlgorithm::Sha256 => "sha256",
            SumAlgorithm::Sha512 => "sha512",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == s)
    }

    /// Hex digits in a digest of this algorithm.
    pub const fn hex_len(self) -> usize {
        match self {
            SumAlgorithm::Sha1 => 40,
            SumAlgorithm::Md5 => 32,
            SumAlgorithm::Sha256 => 64,
            SumAlgorithm::Sha512 => 128,
        }
    }
}

/// The four digests computed over bytes the server stored; lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Digests {
    pub sha1: String,
    pub md5: String,
    pub sha256: String,
    pub sha512: String,
}

impl Digests {
    pub fn get(&self, algorithm: SumAlgorithm) -> &str {
        match algorithm {
            SumAlgorithm::Sha1 => &self.sha1,
            SumAlgorithm::Md5 => &self.md5,
            SumAlgorithm::Sha256 => &self.sha256,
            SumAlgorithm::Sha512 => &self.sha512,
        }
    }
}

/// Which unit: `build` is empty for a release and for a snapshot file
/// deposited without a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitKey<'a> {
    pub repository: i64,
    pub ga: &'a str,
    pub version: &'a str,
    pub build: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub filename: String,
    pub physical_key: String,
    pub size: i64,
    pub digests: Digests,
    pub depositor: String,
    pub created_at: DateTime<Utc>,
}

/// A checksum a client uploaded for one file of the unit: a claim to check,
/// never a value served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub filename: String,
    pub algorithm: SumAlgorithm,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub version: String,
    pub build: String,
    pub revision: i64,
    pub depositor: String,
    pub contested: bool,
    pub refused: bool,
    pub visible_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub files: Vec<StoredFile>,
    pub declarations: Vec<Declaration>,
}

impl Unit {
    pub fn file(&self, filename: &str) -> Option<&StoredFile> {
        self.files.iter().find(|f| f.filename == filename)
    }

    pub fn visible(&self) -> bool {
        self.visible_at.is_some() && !self.refused
    }
}

pub struct NewFile<'a> {
    pub filename: &'a str,
    pub physical_key: &'a str,
    pub size: i64,
    pub digests: &'a Digests,
    pub depositor: &'a str,
}

/// One compare-and-set on a unit.
pub struct UnitChange<'a> {
    pub key: UnitKey<'a>,
    /// The revision the change was decided on; `None` creates the unit.
    pub revision: Option<i64>,
    /// The first depositor, recorded when the change creates the unit.
    pub depositor: &'a str,
    /// Inserted, or replacing the file of the same name: the replaced key is
    /// released and the declarations about the replaced bytes are dropped.
    pub file: Option<NewFile<'a>>,
    pub declarations: &'a [Declaration],
    /// Sets the mark; nothing clears it.
    pub contest: bool,
    /// Makes the unit visible at `now`; a visible unit stays so.
    pub reveal: bool,
    /// The metadata counters this change moves.
    pub scopes: &'a [String],
    pub pins: &'a [PinToken],
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub revision: i64,
    /// Keys no row references any more, enqueued in the change's transaction.
    pub released: Vec<String>,
}

/// One unit of an artifact, as the metadata renderer reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitView {
    pub version: String,
    pub build: String,
    pub visible_at: Option<DateTime<Utc>>,
    pub refused: bool,
    pub files: Vec<StoredFile>,
}

impl UnitView {
    pub fn visible(&self) -> bool {
        self.visible_at.is_some() && !self.refused
    }
}

/// How far a scope's rendering has moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counter {
    pub value: i64,
    pub updated_at: Option<DateTime<Utc>>,
}

/// What a client deposited as `maven-metadata.xml` in one directory: the
/// digests of its body, to check the sums it sends next, and the hints the
/// renderer may keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMetadata {
    pub repository: i64,
    pub dir: String,
    pub digests: Digests,
    pub release: Option<String>,
    pub latest: Option<String>,
    /// `(prefix, artifactId, name)` of a group-level document.
    pub plugins: Vec<(String, String, String)>,
}

/// A unit that is neither visible nor refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUnit {
    pub repository: i64,
    pub ga: String,
    pub version: String,
    pub build: String,
    pub created_at: DateTime<Utc>,
}

/// A version with a visible unit that no `versions` row is known to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unversioned {
    pub repository: i64,
    pub ga: String,
    pub version: String,
}

#[async_trait]
pub trait MavenFileStore: Send + Sync {
    async fn unit(&self, key: &UnitKey<'_>) -> Result<Option<Unit>, StoreError>;

    /// `Conflict` when the revision moved (or the unit exists and `revision`
    /// is `None`); `Superseded` naming the revoked keys when a pin was
    /// revoked. Either way nothing is written.
    async fn change(&self, change: &UnitChange<'_>) -> Result<Changed, StoreError>;

    /// Refuses a unit: its files go, their keys are enqueued and returned,
    /// the unit stays refused. `Conflict` when the revision moved.
    async fn refuse(
        &self,
        key: &UnitKey<'_>,
        revision: i64,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<Changed, StoreError>;

    /// Every unit of `ga`, in no particular order.
    async fn artifact(&self, repository: i64, ga: &str) -> Result<Vec<UnitView>, StoreError>;

    async fn counter(&self, repository: i64, scope: &str) -> Result<Counter, StoreError>;

    /// Replaces the directory's document and moves the counters of `scopes`.
    async fn record_client_metadata(
        &self,
        metadata: &ClientMetadata,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn client_metadata(
        &self,
        repository: i64,
        dir: &str,
    ) -> Result<Option<ClientMetadata>, StoreError>;

    /// Units created before `before` that nothing but the window holds
    /// back: neither visible, refused nor contested, with a file and no
    /// declaration waiting for its file. Oldest first.
    async fn pending(
        &self,
        before: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<PendingUnit>, StoreError>;

    /// Values strictly after `after` in (repository, ga, version) order,
    /// so a pass can page past values that keep failing.
    async fn unversioned(
        &self,
        after: Option<&Unversioned>,
        limit: u32,
    ) -> Result<Vec<Unversioned>, StoreError>;

    async fn mark_versioned(
        &self,
        repository: i64,
        ga: &str,
        version: &str,
    ) -> Result<(), StoreError>;
}
