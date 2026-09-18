//! Publishing one version of a package.
//!
//! npm, cargo and go differ in what they parse, what they checksum and where
//! they file the blob, and not at all in the order those land: the artifact
//! first, then the rows that claim it. One use case therefore serves the
//! three, and the differences ride in the command — a second copy of this
//! ordering per format would be three chances to get it wrong.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::error::{AppError, StoreError};
use crate::ports::packages::{NameMatch, NewRelease, PackageStore, Release};
use crate::storage::{StorageBackend, StorageError};

/// How a publish refuses. The two are kept apart because a full disk and a
/// duplicate version are not the same answer to the client.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<PublishError> for AppError {
    fn from(err: PublishError) -> Self {
        match err {
            PublishError::Store(err) => err.into(),
            PublishError::Storage(err) => err.into(),
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
    pub storage_path: &'a str,
    /// The tags that point at this version once it exists.
    pub dist_tags: &'a [String],
    pub bytes: Bytes,
}

pub struct PublishVersion {
    packages: Arc<dyn PackageStore>,
    storage: Arc<dyn StorageBackend>,
}

impl PublishVersion {
    pub fn new(packages: Arc<dyn PackageStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { packages, storage }
    }

    /// The blob, then the package, version and dist-tag rows in one
    /// transaction.
    ///
    /// This way round because the version row carries the key: a failed
    /// transaction leaves a file no row claims, which costs space, while
    /// rows-first would leave a version whose artifact cannot be downloaded.
    /// Writing the same bytes to the same key twice is a no-op, so a retried
    /// publish repeats the first step harmlessly.
    pub async fn run(
        &self,
        artifact: Artifact<'_>,
        now: DateTime<Utc>,
    ) -> Result<Release, PublishError> {
        let size = artifact.bytes.len() as i64;
        self.storage
            .put(artifact.storage_path, artifact.bytes.clone())
            .await?;

        let landed = self
            .packages
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
                tarball_path: artifact.storage_path,
                dist_tags: artifact.dist_tags,
                now,
            })
            .await?;
        Ok(landed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fakes::{FakeDb, PortId};

    fn artifact<'a>(package: &'a str, version: &'a str, tags: &'a [String]) -> Artifact<'a> {
        Artifact {
            repository: 1,
            package,
            match_name: NameMatch::Exact,
            description: Some("a package"),
            readme: None,
            version,
            metadata_json: "{}",
            checksum_sha1: None,
            checksum_sha256: None,
            integrity: None,
            storage_path: "npm/r/p/p-1.0.0.tgz",
            dist_tags: tags,
            bytes: Bytes::from_static(b"tarball"),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::UNIX_EPOCH
    }

    fn use_case(db: &FakeDb, root: &tempfile::TempDir) -> PublishVersion {
        PublishVersion::new(
            db.packages(),
            crate::server::filesystem(root.path().to_str().unwrap()),
        )
    }

    #[tokio::test]
    async fn a_second_publish_of_the_same_version_is_a_conflict() {
        let db = FakeDb::new();
        let root = tempfile::TempDir::new().unwrap();
        let tags = vec!["latest".to_string()];
        let publish = use_case(&db, &root);

        publish.run(artifact("left-pad", "1.0.0", &tags), now()).await.unwrap();
        let refused = publish
            .run(artifact("left-pad", "1.0.0", &tags), now())
            .await
            .unwrap_err();

        assert!(matches!(refused, PublishError::Store(StoreError::Conflict)));
        assert!(matches!(AppError::from(refused), AppError::Conflict(_)));
    }

    /// The package row is upserted inside the transaction, so a refused
    /// version leaves no package behind for the search index to find.
    #[tokio::test]
    async fn a_refused_write_leaves_no_package_row() {
        let db = FakeDb::new();
        let root = tempfile::TempDir::new().unwrap();
        db.fail_next(PortId::Packages, StoreError::Unavailable);

        let refused = use_case(&db, &root)
            .run(artifact("left-pad", "1.0.0", &[]), now())
            .await
            .unwrap_err();

        assert!(matches!(
            refused,
            PublishError::Store(StoreError::Unavailable)
        ));
        assert!(db
            .packages()
            .package(1, "left-pad", NameMatch::Exact)
            .await
            .unwrap()
            .is_none());
    }
}
