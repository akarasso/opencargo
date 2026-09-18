//! Depositing into a hosted Maven repository: a file, a checksum, or the
//! client's `maven-metadata.xml`.
//!
//! A file's digest is unknown until its body ends, so the body lands in a
//! pinned private draft first; only then is its `HostedKey` known, pinned
//! and the draft relocated to it by `place_shared`, whose commit is the
//! unit's compare-and-set. The rules are decided on the unit as read and
//! decided again whenever the compare-and-set loses.

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tracing::warn;

use super::rules::{judge_file, judge_sum, FileVerdict, Refusal, SumVerdict};
use super::versions::{MavenVersions, VersionError};
use crate::app::place::{Entry, PlaceError, Placer, Source};
use crate::app::publish::{repo_prefix, PublishError};
use crate::domain::layout;
use crate::error::{AppError, StoreError};
use crate::ports::maven::{
    ClientMetadata, Declaration, Digests, MavenFileStore, NewFile, SumAlgorithm, Unit, UnitChange,
    UnitKey,
};
use crate::ports::repositories::RepositoryStore;
use crate::storage::{StorageBackend, StorageError};

/// A request body, as the HTTP adapter hands it over.
pub type Body = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Compare-and-set rounds before a deposit gives up as retryable.
const ROUNDS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deposited {
    /// A new file or declaration; `revealed` when it made its unit visible.
    Stored { revealed: bool },
    /// The same bytes or the same checksum were already there.
    Unchanged,
}

#[derive(Debug, thiserror::Error)]
pub enum DepositError {
    #[error("the version is contested: another principal deposited it first")]
    Contested,
    #[error("{}", .0.reason())]
    Refused(Refusal),
    #[error("the {} checksum does not match the file", .0.as_str())]
    Mismatch(SumAlgorithm),
    #[error("{0}")]
    BadRequest(String),
    #[error("the repository was removed")]
    Retired,
    #[error("the deposit could not be completed, try again")]
    Unavailable,
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<StoreError> for DepositError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Conflict | StoreError::Superseded(_) => DepositError::Unavailable,
            other => DepositError::Store(other),
        }
    }
}

impl From<PlaceError> for DepositError {
    fn from(err: PlaceError) -> Self {
        match PublishError::from(err) {
            PublishError::Store(e) => e.into(),
            PublishError::Storage(e) => DepositError::Storage(e),
            PublishError::Retired => DepositError::Retired,
            PublishError::Unavailable => DepositError::Unavailable,
        }
    }
}

impl From<VersionError> for DepositError {
    fn from(err: VersionError) -> Self {
        match err {
            VersionError::Store(e) => e.into(),
            VersionError::Storage(e) => DepositError::Storage(e),
        }
    }
}

impl From<DepositError> for AppError {
    fn from(err: DepositError) -> Self {
        let message = err.to_string();
        match err {
            DepositError::Contested | DepositError::Refused(_) => AppError::Conflict(message),
            DepositError::Mismatch(_) | DepositError::BadRequest(_) => AppError::BadRequest(message),
            DepositError::Retired => AppError::NotFound(message),
            DepositError::Unavailable => AppError::ServiceUnavailable(message),
            DepositError::Store(e) => e.into(),
            DepositError::Storage(e) => e.into(),
        }
    }
}

/// Where a deposit lands and who makes it.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub unit: UnitKey<'a>,
    /// `group/as/path/artifact`: the package segment of the `HostedKey`.
    pub package_path: &'a str,
    pub filename: &'a str,
    pub principal: &'a str,
    /// The metadata counters a change here moves.
    pub scopes: &'a [String],
}

/// Every digest of a stream, computed as it passes.
#[derive(Default)]
pub struct Hashers {
    sha1: Sha1,
    md5: Md5,
    sha256: Sha256,
    sha512: Sha512,
}

impl Hashers {
    pub fn update(&mut self, chunk: &[u8]) {
        self.sha1.update(chunk);
        self.md5.update(chunk);
        self.sha256.update(chunk);
        self.sha512.update(chunk);
    }

    pub fn finish(self) -> Digests {
        Digests {
            sha1: format!("{:x}", self.sha1.finalize()),
            md5: format!("{:x}", self.md5.finalize()),
            sha256: format!("{:x}", self.sha256.finalize()),
            sha512: format!("{:x}", self.sha512.finalize()),
        }
    }
}

pub fn digests_of(bytes: &[u8]) -> Digests {
    let mut h = Hashers::default();
    h.update(bytes);
    h.finish()
}

pub struct MavenDeposits {
    maven: Arc<dyn MavenFileStore>,
    repos: Arc<dyn RepositoryStore>,
    storage: Arc<dyn StorageBackend>,
    placer: Arc<Placer>,
    versions: Arc<MavenVersions>,
}

impl MavenDeposits {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        storage: Arc<dyn StorageBackend>,
        placer: Arc<Placer>,
        versions: Arc<MavenVersions>,
    ) -> Self {
        Self {
            maven: versions.maven.clone(),
            repos,
            storage,
            placer,
            versions,
        }
    }

    /// One file: artifact, classifier, POM or signature.
    pub async fn file(
        &self,
        target: Target<'_>,
        body: Body,
        now: DateTime<Utc>,
    ) -> Result<Deposited, DepositError> {
        let pom = target.filename.ends_with(".pom");
        let prefix = repo_prefix(self.repos.as_ref(), target.unit.repository)
            .await
            .map_err(|e| match e {
                PublishError::Retired => DepositError::Retired,
                PublishError::Store(e) => e.into(),
                PublishError::Storage(e) => DepositError::Storage(e),
                PublishError::Unavailable => DepositError::Unavailable,
            })?;
        let draft = self.placer.draft(&prefix, now).await?;
        let written = self.write_draft(&draft, body).await;
        let (size, digests) = match written {
            Ok(done) => done,
            Err(e) => {
                self.placer.drop_draft(&draft).await;
                return Err(e);
            }
        };
        let current = self.maven.unit(&target.unit).await?;
        let first = judge_file(current.as_ref(), target.filename, &digests, target.principal, pom);
        let outcome = if matches!(first, FileVerdict::Store { .. }) {
            let placed = self.place(&prefix, &draft, target, size, &digests, now).await;
            self.placer.drop_draft(&draft).await;
            match placed {
                Ok(revealed) => Ok(Deposited::Stored { revealed }),
                Err(DepositError::Unavailable) => self.settle_without_bytes(target, &digests, now).await,
                Err(e) => Err(e),
            }
        } else {
            self.placer.drop_draft(&draft).await;
            self.settle_without_bytes(target, &digests, now).await
        }?;
        if outcome == (Deposited::Stored { revealed: true }) {
            self.publish(target, now).await;
        }
        Ok(outcome)
    }

    async fn write_draft(&self, draft: &str, mut body: Body) -> Result<(i64, Digests), DepositError> {
        let mut writer = self.storage.writer(draft).await?;
        let mut hashers = Hashers::default();
        let mut size: u64 = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| DepositError::BadRequest(format!("failed to read body: {e}")))?;
            size += chunk.len() as u64;
            if size > MAX_FILE_BYTES {
                return Err(DepositError::BadRequest(format!(
                    "the file exceeds {MAX_FILE_BYTES} bytes"
                )));
            }
            hashers.update(&chunk);
            writer.reserve(chunk.len()).await?;
            writer.write(chunk).await?;
        }
        writer.commit().await?;
        Ok((size as i64, hashers.finish()))
    }

    /// The draft moved to its `HostedKey`, the unit changed under its pin.
    async fn place(
        &self,
        prefix: &str,
        draft: &str,
        target: Target<'_>,
        size: i64,
        digests: &Digests,
        now: DateTime<Utc>,
    ) -> Result<bool, DepositError> {
        let entries = [Entry {
            logical_key: layout::hosted_key(prefix, target.package_path, &digests.sha256, target.filename),
            source: Source::Draft(draft.to_string()),
        }];
        let maven = &self.maven;
        let pom = target.filename.ends_with(".pom");
        let revealed = self
            .placer
            .place_shared(
                prefix,
                &entries,
                |pins| async move {
                    for _ in 0..ROUNDS {
                        let unit = maven.unit(&target.unit).await?;
                        let FileVerdict::Store { reveal, .. } =
                            judge_file(unit.as_ref(), target.filename, digests, target.principal, pom)
                        else {
                            return Err(StoreError::Conflict);
                        };
                        let change = UnitChange {
                            key: target.unit,
                            revision: unit.as_ref().map(|u| u.revision),
                            depositor: target.principal,
                            file: Some(NewFile {
                                filename: target.filename,
                                physical_key: &pins[0].physical_key,
                                size,
                                digests,
                                depositor: target.principal,
                            }),
                            declarations: &[],
                            contest: false,
                            reveal,
                            scopes: target.scopes,
                            pins: &pins,
                            now,
                        };
                        match maven.change(&change).await {
                            Ok(_) => return Ok(reveal),
                            Err(StoreError::Conflict) => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Err(StoreError::Conflict)
                },
                now,
            )
            .await?;
        Ok(revealed)
    }

    /// What a deposit whose bytes are not stored answers: the verdict on a
    /// fresh read, with the contest mark written when that is the verdict.
    async fn settle_without_bytes(
        &self,
        target: Target<'_>,
        digests: &Digests,
        now: DateTime<Utc>,
    ) -> Result<Deposited, DepositError> {
        let pom = target.filename.ends_with(".pom");
        for _ in 0..ROUNDS {
            let unit = self.maven.unit(&target.unit).await?;
            match judge_file(unit.as_ref(), target.filename, digests, target.principal, pom) {
                FileVerdict::Identical => return Ok(Deposited::Unchanged),
                FileVerdict::Refuse(why) => return Err(DepositError::Refused(why)),
                FileVerdict::Mismatch(algorithm) => return Err(DepositError::Mismatch(algorithm)),
                FileVerdict::Store { .. } => return Err(DepositError::Unavailable),
                FileVerdict::Contest => {
                    if self.contest(target, unit.as_ref(), now).await? {
                        return Err(DepositError::Contested);
                    }
                }
            }
        }
        Err(DepositError::Unavailable)
    }

    /// `false` when the compare-and-set lost and the caller must decide again.
    async fn contest(&self, target: Target<'_>, unit: Option<&Unit>, now: DateTime<Utc>) -> Result<bool, DepositError> {
        let change = UnitChange {
            key: target.unit,
            revision: unit.map(|u| u.revision),
            depositor: target.principal,
            file: None,
            declarations: &[],
            contest: true,
            reveal: false,
            scopes: target.scopes,
            pins: &[],
            now,
        };
        match self.maven.change(&change).await {
            Ok(_) => Ok(true),
            Err(StoreError::Conflict) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// A checksum the client sent for one file of the unit.
    pub async fn sum(
        &self,
        target: Target<'_>,
        algorithm: SumAlgorithm,
        value: &str,
        now: DateTime<Utc>,
    ) -> Result<Deposited, DepositError> {
        for _ in 0..ROUNDS {
            let unit = self.maven.unit(&target.unit).await?;
            let verdict = judge_sum(unit.as_ref(), target.filename, algorithm, value, target.principal);
            let declared = [Declaration {
                filename: target.filename.to_string(),
                algorithm,
                value: value.to_string(),
            }];
            let (declarations, contest): (&[Declaration], bool) = match verdict {
                SumVerdict::Record => (&declared, false),
                SumVerdict::Contest => (&[], true),
                SumVerdict::Agrees => return Ok(Deposited::Unchanged),
                SumVerdict::Refuse(why) => return Err(DepositError::Refused(why)),
                SumVerdict::Mismatch => return Err(DepositError::Mismatch(algorithm)),
            };
            let change = UnitChange {
                key: target.unit,
                revision: unit.as_ref().map(|u| u.revision),
                depositor: target.principal,
                file: None,
                declarations,
                contest,
                reveal: false,
                scopes: target.scopes,
                pins: &[],
                now,
            };
            match self.maven.change(&change).await {
                Ok(_) if contest => return Err(DepositError::Contested),
                Ok(_) => return Ok(Deposited::Stored { revealed: false }),
                Err(StoreError::Conflict) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(DepositError::Unavailable)
    }

    /// The client's document: its digests to check the sums that follow,
    /// and the hints the renderer may keep. Never served as sent.
    pub async fn client_metadata(
        &self,
        metadata: &ClientMetadata,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<Deposited, DepositError> {
        self.maven.record_client_metadata(metadata, scopes, now).await?;
        Ok(Deposited::Stored { revealed: false })
    }

    /// A checksum of the client's document, checked against it.
    pub async fn client_metadata_sum(
        &self,
        repository: i64,
        dir: &str,
        algorithm: SumAlgorithm,
        value: &str,
    ) -> Result<Deposited, DepositError> {
        match self.maven.client_metadata(repository, dir).await? {
            Some(doc) if doc.digests.get(algorithm) == value => Ok(Deposited::Unchanged),
            Some(_) => Err(DepositError::Mismatch(algorithm)),
            None => Err(DepositError::BadRequest(
                "no maven-metadata.xml was deposited in this directory".to_string(),
            )),
        }
    }

    /// The base version on a `versions` row. The file is already served:
    /// a failure here is repaired by the reconciler, never the client's.
    async fn publish(&self, target: Target<'_>, now: DateTime<Utc>) {
        let unit = match self.maven.unit(&target.unit).await {
            Ok(Some(unit)) => unit,
            Ok(None) => return,
            Err(e) => {
                warn!(error = %e, "maven: the unit could not be read back; the reconciler will publish it");
                return;
            }
        };
        let versioned = self
            .versions
            .ensure(target.unit.repository, target.unit.ga, &unit, now)
            .await
            .map_err(DepositError::from);
        if let Err(e) = versioned {
            warn!(error = %e, ga = target.unit.ga, version = target.unit.version, "maven: version not published yet; the reconciler will");
        }
    }
}
