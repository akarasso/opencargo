//! Port 24: the files of a raw repository, keyed by the path a client asked
//! for.
//!
//! A raw repository has no packages and no versions: one path holds one
//! file, whose physical key a placement pinned and this port records as
//! given. A write that replaces a path releases the key the row held before
//! and enqueues it in the same transaction; nothing here touches storage.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;
use crate::ports::reclaim::PinToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFile {
    pub repository: i64,
    pub path: String,
    pub physical_key: String,
    pub size: i64,
    pub sha256: String,
    pub content_type: Option<String>,
    pub uploaded_by: String,
    pub uploaded_at: DateTime<Utc>,
}

/// One file to record; `pins` carries the single token its key was placed
/// under.
pub struct NewRawFile<'a> {
    pub repository: i64,
    pub path: &'a str,
    pub size: i64,
    pub sha256: &'a str,
    pub content_type: Option<&'a str>,
    pub uploaded_by: &'a str,
    pub pins: &'a [PinToken],
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub file: RawFile,
    /// Whether the path held no file before.
    pub created: bool,
    /// The key the replaced row held, enqueued in the same transaction.
    pub released: Vec<String>,
}

#[async_trait]
pub trait RawFileStore: Send + Sync {
    /// The row for `path`, created or replaced, in one transaction that
    /// spends the pin first; `Superseded` when it was revoked, which writes
    /// nothing.
    async fn put_file(&self, file: &NewRawFile<'_>) -> Result<Stored, StoreError>;

    async fn file(&self, repository: i64, path: &str) -> Result<Option<RawFile>, StoreError>;

    /// The files whose path is `prefix` or lies under it, by path, at most
    /// `limit` of them.
    async fn list(
        &self,
        repository: i64,
        prefix: &str,
        limit: i64,
    ) -> Result<Vec<RawFile>, StoreError>;

    /// Drops the row and enqueues the key it held, in one transaction; the
    /// key comes back as a reclamation candidate and is never deleted here.
    /// `NotFound` when the path holds nothing.
    async fn delete_file(
        &self,
        repository: i64,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError>;
}
