//! Moving a published version from one hosted repository to another.
//!
//! The target owns its artifact under a `HostedKey` of its own incarnation,
//! placed by `place_shared` from the source object with a server-side copy,
//! outside the metadata transaction that spends the pin. A legacy source row
//! with no recorded sha256 is first streamed through a hasher into a private
//! draft, which is then moved into place. Nothing is ever deleted on failure:
//! the target generation is enqueued.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::app::place::{Entry, Placer, Source};
use crate::app::publish::repo_prefix;
use crate::domain::{layout, Repository, Version};
use crate::error::StoreError;
use crate::ports::packages::{PackageStore, Promotion, PromotionAudit};
use crate::ports::repositories::RepositoryStore;
use crate::storage::{StorageBackend, StorageError};

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
    /// The tags the source version holds, which the promoted one inherits.
    pub dist_tags: &'a [String],
    pub details_json: &'a str,
}

pub struct PromoteVersion {
    packages: Arc<dyn PackageStore>,
    repos: Arc<dyn RepositoryStore>,
    storage: Arc<dyn StorageBackend>,
    placer: Arc<Placer>,
}

const DRAFT_CHUNK: usize = 1024 * 1024;

impl PromoteVersion {
    pub fn new(
        packages: Arc<dyn PackageStore>,
        repos: Arc<dyn RepositoryStore>,
        storage: Arc<dyn StorageBackend>,
        placer: Arc<Placer>,
    ) -> Self {
        Self {
            packages,
            repos,
            storage,
            placer,
        }
    }

    /// Place the target generation, then write the package row, the
    /// version, its inherited dist-tags and the audit entry in one
    /// transaction that spends the pin.
    pub async fn run(
        &self,
        request: Request<'_>,
        by: Promoter<'_>,
        now: DateTime<Utc>,
    ) -> Result<Version, PromoteError> {
        let prefix = repo_prefix(self.repos.as_ref(), request.target.id).await?;
        let filename = layout::file_name(layout::logical_key(&request.source.tarball_path));
        let (digest, source, draft) = match request.source.checksum_sha256.as_deref() {
            Some(digest) => (
                digest.to_string(),
                Source::Copy(request.source.tarball_path.clone()),
                None,
            ),
            None => {
                let (draft, digest) = self.draft(&prefix, &request.source.tarball_path, now).await?;
                (digest, Source::Draft(draft.clone()), Some(draft))
            }
        };
        let entries = [Entry {
            logical_key: layout::hosted_key(&prefix, request.package, &digest, filename),
            source,
        }];

        let target = format!("{}@{}", request.package, request.source.version);
        let request = &request;
        let (by, target) = (&by, &target);
        let landed = self
            .placer
            .place_shared(
                &prefix,
                &entries,
                |pins| {
                    let packages = self.packages.clone();
                    async move {
                        packages
                            .promote_metadata(&Promotion {
                                source: request.source,
                                target_repository: request.target.id,
                                package: request.package,
                                description: request.description,
                                metadata_json: request.metadata_json,
                                tarball_path: &pins[0].physical_key,
                                dist_tags: request.dist_tags,
                                pins: &pins,
                                audit: PromotionAudit {
                                    user_id: by.user_id,
                                    username: by.username,
                                    target,
                                    repository: &request.target.name,
                                    details_json: request.details_json,
                                },
                                now,
                            })
                            .await
                    }
                },
                now,
            )
            .await;
        if let Some(draft) = draft {
            self.placer.drop_draft(&draft).await;
        }
        Ok(landed?)
    }

    /// A pinned private draft holding the source bytes, and their sha256.
    async fn draft(
        &self,
        prefix: &str,
        source: &str,
        now: DateTime<Utc>,
    ) -> Result<(String, String), PromoteError> {
        let draft = self.placer.draft(prefix, now).await?;
        let mut read = self.storage.read_stream(source).await?;
        let mut writer = self.storage.writer(&draft).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; DRAFT_CHUNK];
        loop {
            let n = read
                .body
                .read(&mut buf)
                .await
                .map_err(|_| StorageError::Unavailable)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.reserve(n).await?;
            writer.write(Bytes::copy_from_slice(&buf[..n])).await?;
        }
        writer.commit().await?;
        Ok((draft, format!("{:x}", hasher.finalize())))
    }
}

/// The refusal a caller turns into its own words.
pub fn is_conflict(err: &PromoteError) -> bool {
    matches!(err, PromoteError::Store(StoreError::Conflict))
}

#[cfg(test)]
#[path = "promote_tests.rs"]
mod tests;
