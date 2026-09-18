//! Publishing one version of a package.
//!
//! npm, cargo and go differ in what they parse and what they checksum, and
//! not at all in the order the version lands: its bytes under a `HostedKey`
//! of the repository's incarnation, placed by `place_shared`, then the rows
//! that claim them, which spend the placement's pin in their transaction.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::app::place::{Entry, PlaceError, Placer, Source};
use crate::domain::layout;
use crate::error::{AppError, StoreError};
use crate::ports::packages::{NameMatch, NewRelease, PackageStore, Release};
use crate::ports::repositories::RepositoryStore;
use crate::storage::StorageError;

/// How a publish refuses. A full disk, a duplicate version and a placement
/// that must be retried are not the same answer to the client.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error("the repository was removed")]
    Retired,

    #[error("the placement was superseded and could not be replayed, try again")]
    Unavailable,
}

impl From<PlaceError> for PublishError {
    fn from(err: PlaceError) -> Self {
        match err {
            PlaceError::Retired => PublishError::Retired,
            PlaceError::Refused(err) | PlaceError::Store(err) => PublishError::Store(err),
            PlaceError::Unavailable => PublishError::Unavailable,
            PlaceError::Storage(err) => PublishError::Storage(err),
        }
    }
}

impl From<PublishError> for AppError {
    fn from(err: PublishError) -> Self {
        match err {
            PublishError::Store(err) => err.into(),
            PublishError::Storage(err) => err.into(),
            PublishError::Retired => AppError::NotFound(err.to_string()),
            PublishError::Unavailable => AppError::ServiceUnavailable(err.to_string()),
        }
    }
}

/// One version, already validated, checksummed and gated by its format.
pub struct Artifact<'a> {
    pub repository: i64,
    pub package: &'a str,
    pub match_name: NameMatch,
    /// Set on the package row only when this publish creates it.
    pub description: Option<&'a str>,
    pub readme: Option<&'a str>,
    pub version: &'a str,
    pub metadata_json: &'a str,
    pub checksum_sha1: Option<&'a str>,
    pub checksum_sha256: Option<&'a str>,
    pub integrity: Option<&'a str>,
    /// The name the file keeps inside its `HostedKey`.
    pub filename: &'a str,
    /// The tags that point at this version once it exists.
    pub dist_tags: &'a [String],
    pub bytes: Bytes,
}

/// The incarnation prefix of a repository, the root of its new keys.
pub async fn repo_prefix(repos: &dyn RepositoryStore, repository: i64) -> Result<String, PublishError> {
    let incarnation = repos
        .incarnation(repository)
        .await?
        .ok_or(PublishError::Retired)?;
    Ok(layout::incarnation_prefix(&incarnation))
}

pub struct PublishVersion {
    packages: Arc<dyn PackageStore>,
    repos: Arc<dyn RepositoryStore>,
    placer: Arc<Placer>,
}

impl PublishVersion {
    pub fn new(
        packages: Arc<dyn PackageStore>,
        repos: Arc<dyn RepositoryStore>,
        placer: Arc<Placer>,
    ) -> Self {
        Self {
            packages,
            repos,
            placer,
        }
    }

    /// The bytes under a fresh generation, then the package, version and
    /// dist-tag rows in one transaction that spends the pin. A `Conflict`
    /// loser enqueues its generation and deletes nothing.
    pub async fn run(
        &self,
        artifact: Artifact<'_>,
        now: DateTime<Utc>,
    ) -> Result<Release, PublishError> {
        let size = artifact.bytes.len() as i64;
        let sha256 = format!("{:x}", Sha256::digest(&artifact.bytes));
        let prefix = repo_prefix(self.repos.as_ref(), artifact.repository).await?;
        let entries = [Entry {
            logical_key: layout::hosted_key(&prefix, artifact.package, &sha256, artifact.filename),
            source: Source::Bytes(artifact.bytes.clone()),
        }];
        let artifact = &artifact;
        let landed = self
            .placer
            .place_shared(
                &prefix,
                &entries,
                |pins| {
                    let packages = self.packages.clone();
                    async move {
                        packages
                            .publish_version(&NewRelease {
                                repository: artifact.repository,
                                package: artifact.package,
                                match_name: artifact.match_name,
                                description: artifact.description,
                                readme: artifact.readme,
                                version: artifact.version,
                                metadata_json: artifact.metadata_json,
                                checksum_sha1: artifact.checksum_sha1,
                                checksum_sha256: artifact.checksum_sha256,
                                integrity: artifact.integrity,
                                size,
                                tarball_path: &pins[0].physical_key,
                                dist_tags: artifact.dist_tags,
                                dependencies: &[],
                                pins: &pins,
                                now,
                            })
                            .await
                    }
                },
                now,
            )
            .await?;
        Ok(landed)
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
