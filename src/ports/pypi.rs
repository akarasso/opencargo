//! Port 15 (A1 C1): the files of a PyPI release, their `.metadata` keys,
//! their yank state and the project listing.
//!
//! A file row references its artifact and, for a wheel, its `.metadata`:
//! both keys are physical keys a placement pinned, recorded as given and
//! never re-derived. Every method that writes more than one row is one
//! transaction; none performs storage I/O.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;
use crate::ports::reclaim::PinToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PypiFile {
    pub id: i64,
    pub repository: i64,
    pub package_id: i64,
    pub version_id: i64,
    /// PEP 503.
    pub project: String,
    /// The release as its version row keys it.
    pub version: String,
    /// Recomposed from canonical parts; the unique key within a repository.
    pub filename: String,
    pub packagetype: String,
    pub sha256: String,
    pub size: i64,
    pub key: String,
    pub metadata_key: Option<String>,
    pub metadata_sha256: Option<String>,
    pub requires_python: Option<String>,
    pub yanked: bool,
    pub yanked_reason: Option<String>,
    pub uploaded_at: DateTime<Utc>,
}

/// One file to record. `pins` carries one token per key, the artifact's
/// first and the `.metadata`'s second when there is one; the row records
/// their physical keys.
pub struct NewPypiFile<'a> {
    pub repository: i64,
    pub project: &'a str,
    /// Set on the package row only when this file creates it.
    pub summary: Option<&'a str>,
    pub version: &'a str,
    /// The version row's document, written only when this file creates it.
    pub metadata_json: &'a str,
    pub filename: &'a str,
    pub packagetype: &'a str,
    pub sha256: &'a str,
    pub size: i64,
    pub metadata_sha256: Option<&'a str>,
    pub requires_python: Option<&'a str>,
    pub pins: &'a [PinToken],
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub version_created: bool,
    pub file: PypiFile,
}

#[async_trait]
pub trait PypiFileStore: Send + Sync {
    /// Package upsert, version get-or-create and the file row, in one
    /// transaction that spends every pin first. `Conflict` when the
    /// repository already holds that filename, `Superseded` when a pin was
    /// revoked; neither writes anything.
    async fn publish_file(&self, file: &NewPypiFile<'_>) -> Result<Published, StoreError>;

    async fn file_by_name(
        &self,
        repository: i64,
        filename: &str,
    ) -> Result<Option<PypiFile>, StoreError>;

    /// Upload order.
    async fn project_files(
        &self,
        repository: i64,
        project: &str,
    ) -> Result<Vec<PypiFile>, StoreError>;

    /// Projects holding at least one file, sorted.
    async fn list_projects(&self, repository: i64) -> Result<Vec<String>, StoreError>;

    /// The version row and every file of it, in one transaction; `NotFound`
    /// when the release has no file.
    async fn set_release_yanked(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        reason: Option<&str>,
        yanked: bool,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// The files, the version row and what hangs off it, in one
    /// transaction that enqueues every released key, artifacts and
    /// `.metadata` alike; the keys are returned as candidates, never deleted.
    async fn delete_release(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError>;

    /// `delete_release` for every release of the project.
    async fn delete_project_files(
        &self,
        repository: i64,
        project: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError>;
}
