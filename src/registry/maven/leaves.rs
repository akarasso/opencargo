//! What one member of a walk answers for a Maven path.

use bytes::Bytes;
use chrono::Utc;
use tokio::io::AsyncReadExt;

use super::hosted::{self, Rendered};
use super::path::{ArtifactFile, Gav, MetadataLevel};
use super::upstream::{sidecar_value, MavenArtifact, MavenUpstream};
use crate::app::maven::deposit::Hashers;
use crate::app::search;
use crate::domain::{CacheRepo, Format, Outcome, Sighting};
use crate::policy::{self, Source};
use crate::ports::maven::SumAlgorithm;
use crate::proxy::engine::Cached;
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Subject, Upstream};

/// A `maven-metadata.xml` as one member serves it.
#[derive(Debug, Clone)]
pub struct Member {
    pub body: Bytes,
    pub etag: String,
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
    pub stale: bool,
}

impl From<Member> for Rendered {
    fn from(m: Member) -> Self {
        Self {
            body: m.body,
            etag: m.etag,
            last_modified: m.last_modified,
            stale: m.stale,
        }
    }
}

impl From<Rendered> for Member {
    fn from(r: Rendered) -> Self {
        Self {
            body: r.body,
            etag: r.etag,
            last_modified: r.last_modified,
            stale: r.stale,
        }
    }
}

pub struct MetadataLeaf {
    pub dir: Vec<String>,
}

#[async_trait::async_trait]
impl Leaf for MetadataLeaf {
    type Out = Member;

    /// A group-level directory listing has no one coordinate: it enumerates.
    fn subject(&self) -> Subject<'_> {
        match super::path::MetadataLevel::of(&self.dir) {
            Some(level) => Subject::built(level.ga()),
            None => Subject::listing(),
        }
    }

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Member>, ResolveError> {
        Ok(match hosted::metadata(cx.maven, cx.packages, member.0.id, &self.dir).await? {
            Some(doc) => Outcome::Found(doc.into()),
            None => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Member>, ResolveError> {
        let artifact = MavenArtifact::Metadata {
            path: format!("{}/{}", self.dir.join("/"), super::path::METADATA),
        };
        Ok(match cx.proxy.fetch(&MavenUpstream, up, member, &artifact).await? {
            Outcome::Found(cached) => {
                let body = cx.proxy.bytes(&cached).await?;
                remember(cx, member, cached.exchanged, &self.dir, &body).await;
                Outcome::Found(Member {
                    etag: format!("\"{}\"", cached.entry.digest.clone().unwrap_or_default()),
                    last_modified: Some(cached.entry.fetched_at),
                    stale: cached.stale,
                    body,
                })
            }
            Outcome::NotFound => Outcome::NotFound,
        })
    }
}

/// Only the artifact level is a package: a snapshot's `maven-metadata.xml` is
/// about one version of one, and the coordinates a search answers with are the
/// `groupId:artifactId` the hosted rows carry.
async fn remember(cx: &Cx<'_>, member: CacheRepo<'_>, exchanged: bool, dir: &[String], body: &Bytes) {
    let Some(MetadataLevel::Artifact { group, artifact }) = MetadataLevel::of(dir) else {
        return;
    };
    let name = format!("{group}:{artifact}");
    let parsed = super::metadata::parse(body).ok();
    let latest = parsed.and_then(|doc| doc.artifact.release.or(doc.artifact.latest));
    let seen = Sighting {
        repository_id: member.0.id,
        format: Format::Maven,
        name: &name,
        description: None,
        latest_version: latest.as_deref(),
    };
    search::remember(cx.cached, exchanged, &seen, Utc::now()).await;
}

fn immutable(file: &ArtifactFile) -> bool {
    !file.gav.is_snapshot() || !file.build.is_empty()
}

/// The file through the engine, verified against the upstream's own `.sha1`
/// when the upstream has one.
async fn fetch_file(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    file: &ArtifactFile,
) -> Result<Outcome<Cached>, ResolveError> {
    let path = file.path();
    let immutable = immutable(file);
    let sidecar = MavenArtifact::Sidecar {
        path: format!("{path}.sha1"),
        immutable,
    };
    let sidecar_sha1 = match cx.proxy.fetch(&MavenUpstream, up, member, &sidecar).await {
        Ok(Outcome::Found(cached)) => sidecar_value(&cx.proxy.bytes(&cached).await?),
        Ok(Outcome::NotFound) => None,
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "maven: no upstream .sha1 to verify against");
            None
        }
    };
    let artifact = MavenArtifact::File {
        path,
        immutable,
        sidecar_sha1,
    };
    Ok(cx.proxy.fetch(&MavenUpstream, up, member, &artifact).await?)
}

pub struct FileLeaf {
    pub file: ArtifactFile,
}

#[async_trait::async_trait]
impl Leaf for FileLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::built(self.file.gav.ga())
    }
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Payload>, ResolveError> {
        let f = &self.file;
        Ok(match hosted::visible_file(cx.maven, member.0.id, &f.gav, &f.build, &f.filename).await? {
            Some(stored) => Outcome::Found(Payload::file(stored.physical_key, stored.size.max(0) as u64)),
            None => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let cached = fetch_file(cx, member, up, &self.file).await?;
        if let Outcome::Found(c) = &cached {
            if is_artifact(&self.file) {
                let gav = &self.file.gav;
                policy::record(cx, member, up, Format::Maven, &gav.ga(), Some(gav.version.clone()), || {
                    Source::Maven {
                        digest: c.entry.digest.clone(),
                    }
                });
            }
        }
        Ok(cached.into_payload())
    }
}

/// The files that are the artifact, not a description or a signature of
/// it: one policy row per served build, as the other formats record their
/// tarball, crate, zip or package.
fn is_artifact(file: &ArtifactFile) -> bool {
    !matches!(file.extension.as_str(), "pom" | "module") && !file.extension.ends_with(".asc")
}

/// A file's checksum, from the member that serves the file: stored digests
/// for a hosted member, the digest of the cached body for a proxy, never an
/// upstream sidecar served as is.
pub struct SumLeaf {
    pub file: ArtifactFile,
    pub algorithm: SumAlgorithm,
}

#[async_trait::async_trait]
impl Leaf for SumLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::built(self.file.gav.ga())
    }
    type Out = String;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<String>, ResolveError> {
        let f = &self.file;
        Ok(match hosted::visible_file(cx.maven, member.0.id, &f.gav, &f.build, &f.filename).await? {
            Some(stored) => Outcome::Found(stored.digests.get(self.algorithm).to_string()),
            None => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<String>, ResolveError> {
        let Outcome::Found(cached) = fetch_file(cx, member, up, &self.file).await? else {
            return Ok(Outcome::NotFound);
        };
        let mut read = cx.proxy.read_stream(&cached).await?;
        let mut hashers = Hashers::default();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = read
                .body
                .read(&mut buf)
                .await
                .map_err(|_| crate::storage::StorageError::Unavailable)?;
            if n == 0 {
                break;
            }
            hashers.update(&buf[..n]);
        }
        Ok(Outcome::Found(hashers.finish().get(self.algorithm).to_string()))
    }
}

/// A group's metadata merge needs the snapshot's coordinates.
pub fn snapshot_of(dir: &[String]) -> Option<Gav> {
    match super::path::MetadataLevel::of(dir)? {
        super::path::MetadataLevel::Snapshot(gav) => Some(gav),
        super::path::MetadataLevel::Artifact { .. } => None,
    }
}
