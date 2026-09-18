//! What one member of a walk answers for a Maven path.

use bytes::Bytes;
use tokio::io::AsyncReadExt;

use super::hosted::{self, Rendered};
use super::path::{ArtifactFile, Gav};
use super::upstream::{sidecar_value, MavenArtifact, MavenUpstream};
use crate::app::maven::deposit::Hashers;
use crate::domain::{CacheRepo, Outcome};
use crate::ports::maven::SumAlgorithm;
use crate::proxy::engine::Cached;
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

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
        Ok(fetch_file(cx, member, up, &self.file).await?.into_payload())
    }
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
