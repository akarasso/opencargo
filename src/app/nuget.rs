//! Pushing a NuGet package: the id and version admitted and normalized by
//! the format's rules, the `.nupkg` placed under a `HostedKey` by
//! `place_shared` only, the version row committed by compare-and-set on the
//! placement's pin. A `Superseded` commit is replayed from the request's
//! spool by the helper; a `Conflict` loser's generation is enqueued; nothing
//! here deletes or pins.

use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256, Sha512};
use tracing::warn;

use crate::app::place::{Entry, Placer, Source};
use crate::app::publish::{repo_prefix, PublishError};
use crate::domain::{layout, DomainError, FormatRules};
use crate::error::AppError;
use crate::ports::deps::{DependencyStore, NewDependency};
use crate::ports::packages::{NameMatch, NewRelease, PackageStore, Release};
use crate::ports::repositories::RepositoryStore;

/// One edge of the nuspec: the dependency id, its range, and its target
/// framework as the edge's kind.
pub struct NugetDependency<'a> {
    pub id: &'a str,
    pub range: &'a str,
    pub framework: &'a str,
}

/// A push as the adapter parsed it: the id and version as the nuspec spells
/// them, and the spool the helper replays from.
pub struct NugetPush<'a> {
    pub repository: i64,
    pub id: &'a str,
    pub version: &'a str,
    pub description: Option<&'a str>,
    pub metadata_json: &'a str,
    pub dependencies: &'a [NugetDependency<'a>],
    pub spool: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(transparent)]
    Invalid(#[from] DomainError),
    #[error(transparent)]
    Publish(#[from] PublishError),
}

impl From<PushError> for AppError {
    fn from(err: PushError) -> Self {
        match err {
            PushError::Invalid(e) => e.into(),
            PushError::Publish(e) => e.into(),
        }
    }
}

pub struct PublishNugetPackage {
    packages: Arc<dyn PackageStore>,
    repos: Arc<dyn RepositoryStore>,
    deps: Arc<dyn DependencyStore>,
    placer: Arc<Placer>,
    rules: &'static dyn FormatRules,
}

impl PublishNugetPackage {
    pub fn new(
        packages: Arc<dyn PackageStore>,
        repos: Arc<dyn RepositoryStore>,
        deps: Arc<dyn DependencyStore>,
        placer: Arc<Placer>,
        rules: &'static dyn FormatRules,
    ) -> Self {
        Self {
            packages,
            repos,
            deps,
            placer,
            rules,
        }
    }

    pub async fn run(&self, push: NugetPush<'_>, now: DateTime<Utc>) -> Result<Release, PushError> {
        let name = self.rules.admit(push.id)?;
        self.rules.validate_version(push.version)?;
        let version = self.rules.normalize_version(push.version);
        let sha256 = format!("{:x}", Sha256::digest(&push.spool));
        let sha512 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&push.spool));
        let size = push.spool.len() as i64;
        let prefix = repo_prefix(self.repos.as_ref(), push.repository).await?;
        let filename = format!("{name}.{version}.nupkg");
        let entries = [Entry {
            logical_key: layout::hosted_key(&prefix, &name, &sha256, &filename),
            source: Source::Bytes(push.spool.clone()),
        }];
        let (name_ref, version_ref, sha256_ref, sha512_ref) = (&name, &version, &sha256, &sha512);
        let push_ref = &push;
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
                                repository: push_ref.repository,
                                package: name_ref,
                                match_name: NameMatch::Exact,
                                description: push_ref.description,
                                readme: None,
                                version: version_ref,
                                metadata_json: push_ref.metadata_json,
                                checksum_sha1: None,
                                checksum_sha256: Some(sha256_ref),
                                integrity: Some(sha512_ref),
                                size,
                                tarball_path: &pins[0].physical_key,
                                dist_tags: &[],
                                pins: &pins,
                                now,
                            })
                            .await
                    }
                },
                now,
            )
            .await
            .map_err(PublishError::from)?;
        for dep in push.dependencies {
            let edge = NewDependency {
                package: landed.package.id,
                version: landed.version.id,
                name: dep.id,
                requirement: dep.range,
                kind: dep.framework,
            };
            if let Err(e) = self.deps.record(&edge, now).await {
                warn!(error = %e, dependency = dep.id, "nuget dependency not recorded");
            }
        }
        Ok(landed)
    }
}

#[cfg(test)]
#[path = "nuget_tests.rs"]
mod tests;
