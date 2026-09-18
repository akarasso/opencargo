//! PyPI's write use cases: a file lands with its `.metadata` in one
//! placement and one commit, a release is yanked or deleted as a whole.
//!
//! Every shared key goes through `place_shared`; nothing here pins, enqueues
//! or deletes. A deleted release's keys are enqueued by the store in the
//! transaction that drops its rows.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::app::place::{Entry, Placer, Source};
use crate::app::publish::{repo_prefix, PublishError};
use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::pypi::{NewPypiFile, Published, PypiFile, PypiFileStore};
use crate::ports::repositories::RepositoryStore;

/// One distribution file, parsed and validated by the format adapter: every
/// name and version is already canonical.
pub struct Upload<'a> {
    pub repository: i64,
    pub project: &'a str,
    pub summary: Option<&'a str>,
    pub version: &'a str,
    pub metadata_json: &'a str,
    pub filename: &'a str,
    pub packagetype: &'a str,
    pub requires_python: Option<&'a str>,
    pub bytes: Bytes,
    /// A wheel's core metadata, served beside it (PEP 658).
    pub metadata: Option<Bytes>,
}

#[derive(Debug)]
pub enum Uploaded {
    Created(Published),
    /// The same bytes under the same name were already there.
    Unchanged(PypiFile),
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct PublishPypiFile {
    files: Arc<dyn PypiFileStore>,
    repos: Arc<dyn RepositoryStore>,
    placer: Arc<Placer>,
}

impl PublishPypiFile {
    pub fn new(
        files: Arc<dyn PypiFileStore>,
        repos: Arc<dyn RepositoryStore>,
        placer: Arc<Placer>,
    ) -> Self {
        Self {
            files,
            repos,
            placer,
        }
    }

    /// The artifact and its `.metadata` as two entries of one placement,
    /// one token each, one `publish_file` commit. A `Conflict` over the same
    /// sha256 is the idempotent success twine's `--skip-existing` relies on.
    pub async fn run(&self, upload: Upload<'_>, now: DateTime<Utc>) -> Result<Uploaded, PublishError> {
        let sha256 = sha256_hex(&upload.bytes);
        let metadata_sha256 = upload.metadata.as_deref().map(sha256_hex);
        let prefix = repo_prefix(self.repos.as_ref(), upload.repository).await?;
        let artifact_key = layout::hosted_key(&prefix, upload.project, &sha256, upload.filename);
        let mut entries = vec![Entry {
            logical_key: artifact_key,
            source: Source::Bytes(upload.bytes.clone()),
        }];
        if let Some(metadata) = &upload.metadata {
            entries.push(Entry {
                logical_key: layout::hosted_key(
                    &prefix,
                    upload.project,
                    &sha256,
                    &format!("{}.metadata", upload.filename),
                ),
                source: Source::Bytes(metadata.clone()),
            });
        }
        let size = upload.bytes.len() as i64;
        let (upload, sha256, metadata_sha256) = (&upload, &sha256, &metadata_sha256);
        let placed = self
            .placer
            .place_shared(
                &prefix,
                &entries,
                |pins| {
                    let files = self.files.clone();
                    async move {
                        files
                            .publish_file(&NewPypiFile {
                                repository: upload.repository,
                                project: upload.project,
                                summary: upload.summary,
                                version: upload.version,
                                metadata_json: upload.metadata_json,
                                filename: upload.filename,
                                packagetype: upload.packagetype,
                                sha256,
                                size,
                                metadata_sha256: metadata_sha256.as_deref(),
                                requires_python: upload.requires_python,
                                pins: &pins,
                                now,
                            })
                            .await
                    }
                },
                now,
            )
            .await;
        match placed {
            Ok(published) => Ok(Uploaded::Created(published)),
            Err(err) => {
                let err = PublishError::from(err);
                if !matches!(err, PublishError::Store(StoreError::Conflict)) {
                    return Err(err);
                }
                match self.files.file_by_name(upload.repository, upload.filename).await? {
                    Some(existing) if existing.sha256 == *sha256 => Ok(Uploaded::Unchanged(existing)),
                    _ => Err(err),
                }
            }
        }
    }
}

pub struct YankRelease {
    files: Arc<dyn PypiFileStore>,
}

impl YankRelease {
    pub fn new(files: Arc<dyn PypiFileStore>) -> Self {
        Self { files }
    }

    pub async fn run(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        reason: Option<&str>,
        yanked: bool,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.files
            .set_release_yanked(repository, project, version, reason, yanked, now)
            .await
    }
}

pub struct DeleteRelease {
    files: Arc<dyn PypiFileStore>,
}

impl DeleteRelease {
    pub fn new(files: Arc<dyn PypiFileStore>) -> Self {
        Self { files }
    }

    /// One release, or every release of the project when `version` is
    /// `None`. The keys come back as the reclamation candidates the store
    /// enqueued; nothing is deleted here.
    pub async fn run(
        &self,
        repository: i64,
        project: &str,
        version: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        match version {
            Some(version) => self.files.delete_release(repository, project, version, now).await,
            None => self.files.delete_project_files(repository, project, now).await,
        }
    }
}

#[cfg(test)]
#[path = "pypi_tests.rs"]
mod tests;
