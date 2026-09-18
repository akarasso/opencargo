//! A visible unit's base version on a `versions` row, so the generic
//! readers (dashboard, search, scans, webhooks) see Maven like any format.
//! Published once per base version: a later build of the same snapshot
//! finds the row there, and a `Conflict` from a racing publisher is a
//! result, never a failed deposit.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::app::publish_tail::{PreScan, PublishTail, Published};
use crate::domain::Format;
use crate::error::StoreError;
use crate::ports::maven::{MavenFileStore, Unit};
use crate::ports::packages::{NameMatch, NewRelease, PackageStore};
use crate::ports::repositories::RepositoryStore;
use crate::storage::{StorageBackend, StorageError};

/// Where a newly published version is announced: the shared publish tail
/// in the server, a recorder in tests.
#[async_trait]
pub trait Announcer: Send + Sync {
    async fn announce(&self, done: &Published<'_>, now: DateTime<Utc>);
}

#[async_trait]
impl Announcer for PublishTail {
    async fn announce(&self, done: &Published<'_>, now: DateTime<Utc>) {
        self.run(done, PreScan::default(), now).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versioned {
    Published,
    Already,
}

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub struct MavenVersions {
    pub(super) maven: Arc<dyn MavenFileStore>,
    pub(super) packages: Arc<dyn PackageStore>,
    pub(super) repos: Arc<dyn RepositoryStore>,
    pub(super) storage: Arc<dyn StorageBackend>,
    pub(super) announcer: Arc<dyn Announcer>,
    /// The POM parser of the protocol adapter: coordinates and dependencies
    /// as the JSON a `versions` row carries.
    pub(super) describe: fn(&[u8], &str, &str, &str) -> String,
}

impl MavenVersions {
    pub fn new(
        maven: Arc<dyn MavenFileStore>,
        packages: Arc<dyn PackageStore>,
        repos: Arc<dyn RepositoryStore>,
        storage: Arc<dyn StorageBackend>,
        announcer: Arc<dyn Announcer>,
        describe: fn(&[u8], &str, &str, &str) -> String,
    ) -> Self {
        Self {
            maven,
            packages,
            repos,
            storage,
            announcer,
            describe,
        }
    }

    /// The row for `unit`'s base version, created when absent; announced
    /// only by the call that created it.
    pub async fn ensure(
        &self,
        repository: i64,
        ga: &str,
        unit: &Unit,
        now: DateTime<Utc>,
    ) -> Result<Versioned, VersionError> {
        let version = unit.version.as_str();
        if let Some(package) = self.packages.package(repository, ga, NameMatch::Exact).await? {
            if self.packages.version(package.id, version).await?.is_some() {
                self.maven.mark_versioned(repository, ga, version).await?;
                return Ok(Versioned::Already);
            }
        }
        let Some(main) = unit
            .files
            .iter()
            .find(|f| f.filename.ends_with(".pom"))
            .or_else(|| unit.files.first())
        else {
            return Ok(Versioned::Already);
        };
        let (group, artifact) = ga.split_once(':').unwrap_or((ga, ""));
        let metadata_json = if main.filename.ends_with(".pom") {
            let pom = self.storage.get(&main.physical_key).await?;
            (self.describe)(&pom, group, artifact, version)
        } else {
            (self.describe)(b"", group, artifact, version)
        };
        let description: Option<String> = serde_json::from_str::<serde_json::Value>(&metadata_json)
            .ok()
            .and_then(|m| m["description"].as_str().map(String::from));
        let integrity = format!("sha256-{}", hex_to_b64(&main.digests.sha256));
        let published = self
            .packages
            .publish_version(&NewRelease {
                repository,
                package: ga,
                match_name: NameMatch::Exact,
                description: description.as_deref(),
                readme: None,
                version,
                metadata_json: &metadata_json,
                checksum_sha1: Some(&main.digests.sha1),
                checksum_sha256: Some(&main.digests.sha256),
                integrity: Some(&integrity),
                size: main.size,
                tarball_path: &main.physical_key,
                dist_tags: &[],
                pins: &[],
                now,
            })
            .await;
        let release = match published {
            Ok(release) => release,
            Err(StoreError::Conflict) => {
                self.maven.mark_versioned(repository, ga, version).await?;
                return Ok(Versioned::Already);
            }
            Err(e) => return Err(e.into()),
        };
        self.maven.mark_versioned(repository, ga, version).await?;
        let name = self
            .repos
            .by_id(repository)
            .await?
            .map(|r| r.name)
            .unwrap_or_default();
        self.announcer
            .announce(
                &Published {
                    format: Format::Maven,
                    repository: &name,
                    package: ga,
                    version,
                    version_id: Some(release.version.id),
                    metadata_json: &metadata_json,
                    published_by: &unit.depositor,
                },
                now,
            )
            .await;
        Ok(Versioned::Published)
    }
}

fn hex_to_b64(hex: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
