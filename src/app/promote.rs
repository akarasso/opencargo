//! Moving a published version from one hosted repository to another.
//!
//! The artifact copy is a full read and write of a blob bounded only by the
//! body cap, so it stays outside the metadata transaction: SQLite has a
//! single writer, and holding it for the length of a gigabyte copy would turn
//! every concurrent publish into a timeout.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::domain::{Repository, Version};
use crate::error::StoreError;
use crate::ports::packages::{PackageStore, Promotion, PromotionAudit};
use crate::storage::StorageBackend;

pub use crate::app::publish::PublishError as PromoteError;

/// Who asked, so the audit row the transaction writes names them.
pub struct Promoter<'a> {
    /// `None` for a static token, which has no user row behind it.
    pub user_id: Option<i64>,
    pub username: &'a str,
}

/// What to promote and where to.
pub struct Request<'a> {
    pub source: &'a Version,
    pub target: &'a Repository,
    pub package: &'a str,
    pub description: Option<&'a str>,
    /// The source metadata with its tarball URL pointed at the target.
    pub metadata_json: &'a str,
    pub target_path: &'a str,
    /// The tags the source version holds, which the promoted one inherits.
    pub dist_tags: &'a [String],
    pub details_json: &'a str,
}

pub struct PromoteVersion {
    packages: Arc<dyn PackageStore>,
    storage: Arc<dyn StorageBackend>,
}

impl PromoteVersion {
    pub fn new(packages: Arc<dyn PackageStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { packages, storage }
    }

    /// Copy the blob, then write the package row, the version, its inherited
    /// dist-tags and the audit entry in one transaction.
    ///
    /// A failed transaction is compensated: the copy is idempotent on a key
    /// that belongs to a version which does not exist, so deleting it is
    /// safe. A crash between the two steps is the residual window, and it
    /// leaks an unreferenced object — nothing reclaims it, because the
    /// storage port cannot yet list what no row claims.
    pub async fn run(
        &self,
        request: Request<'_>,
        by: Promoter<'_>,
        now: DateTime<Utc>,
    ) -> Result<Version, PromoteError> {
        if request.target_path != request.source.tarball_path {
            let bytes = self.storage.get(&request.source.tarball_path).await?;
            self.storage.put(request.target_path, bytes).await?;
        }

        let target = format!("{}@{}", request.package, request.source.version);
        let landed = self
            .packages
            .promote_metadata(&Promotion {
                source: request.source,
                target_repository: request.target.id,
                package: request.package,
                description: request.description,
                metadata_json: request.metadata_json,
                tarball_path: request.target_path,
                dist_tags: request.dist_tags,
                audit: PromotionAudit {
                    user_id: by.user_id,
                    username: by.username,
                    target: &target,
                    repository: &request.target.name,
                    details_json: request.details_json,
                },
                now,
            })
            .await;

        match landed {
            Ok(version) => Ok(version),
            Err(err) => {
                self.compensate(&request).await;
                Err(err.into())
            }
        }
    }

    /// Best effort, and only for the copy this call made: the key belongs to
    /// a version that does not exist, so nothing else can be reading it.
    async fn compensate(&self, request: &Request<'_>) {
        if request.target_path == request.source.tarball_path {
            return;
        }
        if let Err(err) = self.storage.delete(request.target_path).await {
            warn!(
                path = %request.target_path,
                "promotion failed and its copied artifact could not be removed: {err}"
            );
        }
    }
}

/// The refusal a caller turns into its own words.
pub fn is_conflict(err: &PromoteError) -> bool {
    matches!(err, PromoteError::Store(StoreError::Conflict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RepoConfig, Visibility};
    use crate::testing::fakes::{FakeDb, PortId};
    use bytes::Bytes;

    fn version(path: &str) -> Version {
        Version {
            id: 7,
            package_id: 3,
            version: "1.0.0".to_string(),
            metadata_json: "{}".to_string(),
            checksum_sha1: None,
            checksum_sha256: Some("abc".to_string()),
            integrity: None,
            size: 4,
            tarball_path: path.to_string(),
            published_at: DateTime::UNIX_EPOCH,
            yanked: false,
        }
    }

    fn repository() -> Repository {
        Repository {
            id: 2,
            name: "npm-prod".to_string(),
            repo_type: "hosted".to_string(),
            format: "npm".to_string(),
            visibility: Visibility::Public,
            upstream_url: None,
            config: None::<RepoConfig>,
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
        }
    }

    fn request<'a>(source: &'a Version, target: &'a Repository, tags: &'a [String]) -> Request<'a> {
        Request {
            source,
            target,
            package: "left-pad",
            description: Some("a package"),
            metadata_json: "{}",
            target_path: "npm/npm-prod/left-pad/left-pad-1.0.0.tgz",
            dist_tags: tags,
            details_json: r#"{"from":"npm-stage","to":"npm-prod"}"#,
        }
    }

    async fn staged(root: &tempfile::TempDir) -> (Arc<dyn StorageBackend>, Version) {
        let storage = crate::storage::filesystem(root.path().to_str().unwrap());
        let source = version("npm/npm-stage/left-pad/left-pad-1.0.0.tgz");
        storage
            .put(&source.tarball_path, Bytes::from_static(b"tgz!"))
            .await
            .unwrap();
        (storage, source)
    }

    /// The four writes land together, and the audit row names the promoter.
    #[tokio::test]
    async fn a_promotion_carries_the_blob_the_rows_and_its_tags() {
        let root = tempfile::TempDir::new().unwrap();
        let (storage, source) = staged(&root).await;
        let db = FakeDb::new();
        let target = repository();
        let tags = vec!["latest".to_string()];

        let promoted = PromoteVersion::new(db.packages(), storage.clone())
            .run(
                request(&source, &target, &tags),
                Promoter {
                    user_id: Some(1),
                    username: "alex",
                },
                DateTime::UNIX_EPOCH,
            )
            .await
            .unwrap();

        assert_eq!(promoted.tarball_path, "npm/npm-prod/left-pad/left-pad-1.0.0.tgz");
        assert_eq!(promoted.checksum_sha256.as_deref(), Some("abc"));
        assert!(storage.get(&promoted.tarball_path).await.is_ok());

        let package = db.packages().versions(promoted.package_id).await.unwrap();
        assert_eq!(package.len(), 1);
        let tags = db.packages().dist_tags(promoted.package_id).await.unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].version_id, promoted.id);

        assert_eq!(
            db.audit_rows(),
            vec![(Some(1), "package.promote".to_string(), "left-pad@1.0.0".to_string())]
        );
    }

    /// A failed metadata transaction takes its copy back with it, so no
    /// object is left that no row claims.
    #[tokio::test]
    async fn a_failed_promotion_removes_the_artifact_it_copied() {
        let root = tempfile::TempDir::new().unwrap();
        let (storage, source) = staged(&root).await;
        let db = FakeDb::new();
        db.fail_next(PortId::Packages, StoreError::Unavailable);
        let target = repository();

        let refused = PromoteVersion::new(db.packages(), storage.clone())
            .run(
                request(&source, &target, &[]),
                Promoter {
                    user_id: Some(1),
                    username: "alex",
                },
                DateTime::UNIX_EPOCH,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            refused,
            PromoteError::Store(StoreError::Unavailable)
        ));
        assert!(storage
            .get("npm/npm-prod/left-pad/left-pad-1.0.0.tgz")
            .await
            .is_err());
        assert!(storage.get(&source.tarball_path).await.is_ok());
        assert!(db.audit_rows().is_empty());
    }
}
