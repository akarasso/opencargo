//! The write use cases of a raw repository: one path holds one file.
//!
//! A raw body carries no manifest, so its digest is unknown until it ends:
//! the bytes land in a pinned private draft, and only then is the
//! `HostedKey` known, pinned and the draft relocated to it by
//! `place_shared`, whose commit is the row. Nothing here pins, enqueues or
//! deletes a shared key.

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};

use crate::app::place::{Entry, Placer, Source};
use crate::app::publish::{repo_prefix, PublishError};
use crate::domain::layout;
use crate::error::{AppError, StoreError};
use crate::ports::raw::{NewRawFile, RawFile, RawFileStore};
use crate::ports::repositories::RepositoryStore;
use crate::storage::{StorageBackend, StorageError};

/// A request body, as the HTTP adapter hands it over.
pub type Body = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Where a file lands and who deposits it.
pub struct Deposit<'a> {
    pub repository: i64,
    pub path: &'a str,
    pub content_type: Option<&'a str>,
    /// The sha256 the client declared for these bytes, checked against what
    /// was received before anything is recorded.
    pub declared_sha256: Option<&'a str>,
    pub principal: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deposited {
    /// `created` is false when the path held other bytes before.
    Stored { created: bool, file: RawFile },
    /// The same bytes were already at that path.
    Unchanged(RawFile),
}

impl Deposited {
    pub fn file(&self) -> &RawFile {
        match self {
            Deposited::Stored { file, .. } | Deposited::Unchanged(file) => file,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("the sha256 of the body is {got}, not the declared {declared}")]
    Mismatch { declared: String, got: String },
    #[error("{0}")]
    BadRequest(String),
    #[error("the repository was removed")]
    Retired,
    #[error("the upload could not be completed, try again")]
    Unavailable,
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<StoreError> for RawError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Superseded(_) => RawError::Unavailable,
            other => RawError::Store(other),
        }
    }
}

impl From<PublishError> for RawError {
    fn from(err: PublishError) -> Self {
        match err {
            PublishError::Store(e) => e.into(),
            PublishError::Storage(e) => RawError::Storage(e),
            PublishError::Retired => RawError::Retired,
            PublishError::Unavailable => RawError::Unavailable,
        }
    }
}

impl From<crate::app::place::PlaceError> for RawError {
    fn from(err: crate::app::place::PlaceError) -> Self {
        PublishError::from(err).into()
    }
}

impl From<RawError> for AppError {
    fn from(err: RawError) -> Self {
        let message = err.to_string();
        match err {
            RawError::Mismatch { .. } | RawError::BadRequest(_) => AppError::BadRequest(message),
            RawError::Retired => AppError::NotFound(message),
            RawError::Unavailable => AppError::ServiceUnavailable(message),
            RawError::Store(e) => e.into(),
            RawError::Storage(e) => e.into(),
        }
    }
}

pub struct PutRawFile {
    raw: Arc<dyn RawFileStore>,
    repos: Arc<dyn RepositoryStore>,
    storage: Arc<dyn StorageBackend>,
    placer: Arc<Placer>,
}

impl PutRawFile {
    pub fn new(
        raw: Arc<dyn RawFileStore>,
        repos: Arc<dyn RepositoryStore>,
        storage: Arc<dyn StorageBackend>,
        placer: Arc<Placer>,
    ) -> Self {
        Self {
            raw,
            repos,
            storage,
            placer,
        }
    }

    pub async fn run(
        &self,
        deposit: Deposit<'_>,
        body: Body,
        now: DateTime<Utc>,
    ) -> Result<Deposited, RawError> {
        let prefix = repo_prefix(self.repos.as_ref(), deposit.repository).await?;
        let draft = self.placer.draft(&prefix, now).await?;
        let (size, sha256) = match self.write_draft(&draft, body).await {
            Ok(done) => done,
            Err(e) => {
                self.placer.drop_draft(&draft).await;
                return Err(e);
            }
        };
        if let Some(declared) = deposit.declared_sha256 {
            if !declared.eq_ignore_ascii_case(&sha256) {
                self.placer.drop_draft(&draft).await;
                return Err(RawError::Mismatch {
                    declared: declared.to_string(),
                    got: sha256,
                });
            }
        }
        let held = self.raw.file(deposit.repository, deposit.path).await?;
        if let Some(held) = held.filter(|f| f.sha256 == sha256) {
            self.placer.drop_draft(&draft).await;
            return Ok(Deposited::Unchanged(held));
        }
        let placed = self.place(&prefix, &draft, &deposit, size, &sha256, now).await;
        self.placer.drop_draft(&draft).await;
        placed
    }

    async fn write_draft(&self, draft: &str, mut body: Body) -> Result<(i64, String), RawError> {
        let mut writer = self.storage.writer(draft).await?;
        let mut digest = Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| RawError::BadRequest(format!("failed to read body: {e}")))?;
            size += chunk.len() as u64;
            if size > MAX_FILE_BYTES {
                return Err(RawError::BadRequest(format!(
                    "the file exceeds {MAX_FILE_BYTES} bytes"
                )));
            }
            digest.update(&chunk);
            writer.reserve(chunk.len()).await?;
            writer.write(chunk).await?;
        }
        writer.commit().await?;
        Ok((size as i64, format!("{:x}", digest.finalize())))
    }

    async fn place(
        &self,
        prefix: &str,
        draft: &str,
        deposit: &Deposit<'_>,
        size: i64,
        sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<Deposited, RawError> {
        let filename = layout::file_name(deposit.path);
        let entries = [Entry {
            logical_key: layout::hosted_key(prefix, deposit.path, sha256, filename),
            source: Source::Draft(draft.to_string()),
        }];
        let raw = &self.raw;
        let stored = self
            .placer
            .place_shared(
                prefix,
                &entries,
                |pins| async move {
                    raw.put_file(&NewRawFile {
                        repository: deposit.repository,
                        path: deposit.path,
                        size,
                        sha256,
                        content_type: deposit.content_type,
                        uploaded_by: deposit.principal,
                        pins: &pins,
                        now,
                    })
                    .await
                },
                now,
            )
            .await?;
        Ok(Deposited::Stored {
            created: stored.created,
            file: stored.file,
        })
    }
}

pub struct DeleteRawFile {
    raw: Arc<dyn RawFileStore>,
}

impl DeleteRawFile {
    pub fn new(raw: Arc<dyn RawFileStore>) -> Self {
        Self { raw }
    }

    /// The keys come back as the reclamation candidates the store enqueued;
    /// nothing is deleted here.
    pub async fn run(
        &self,
        repository: i64,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        self.raw.delete_file(repository, path, now).await
    }
}

#[cfg(test)]
#[path = "raw_tests.rs"]
mod tests;
