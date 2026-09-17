use crate::db::kinds::Format;
use crate::error::{AppError, AppResult};
use crate::policy::{self, Source};
use crate::proxy::engine::Cached;
use crate::proxy::Payload;
use crate::registry::resolve::{CacheRepo, Cx, Leaf, Outcome, Upstream};

use super::manifests::resolve_hosted_digest;
use super::upstream::{upstream_name, OciArtifact, OciUpstream};
use super::{is_digest, paths};

const OCTET_STREAM: &str = "application/octet-stream";

/// One engine call per artifact: a HEAD is answered from the row or the
/// upstream headers, a GET lands the body first.
async fn from_engine(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &OciArtifact,
    head: bool,
) -> AppResult<Outcome<Payload>> {
    let engine = &cx.state.proxy;
    if head {
        engine.head(&OciUpstream, up, member, a).await
    } else {
        Ok(engine
            .fetch(&OciUpstream, up, member, a)
            .await?
            .into_payload())
    }
}

/// `engine.fetch` without `into_payload`, for the one GET that records.
async fn fetch_cached(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &OciArtifact,
) -> AppResult<Outcome<Cached>> {
    cx.state.proxy.fetch(&OciUpstream, up, member, a).await
}

fn with_digest(mut p: Payload, digest: String) -> Payload {
    p.digest = Some(digest);
    p
}

pub struct BlobLeaf {
    pub name: String,
    pub digest: String,
    pub head: bool,
}

#[async_trait::async_trait]
impl Leaf for BlobLeaf {
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Payload>> {
        let Some(blob) = crate::db::oci::get_blob(&cx.state.db, member.0.id, &self.digest).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let size = blob.size.max(0) as u64;
        let content_type = blob.content_type.or_else(|| Some(OCTET_STREAM.to_string()));
        let payload = if self.head {
            Payload::head_only(size, content_type, Some(self.digest.clone()))
        } else {
            crate::telemetry::record_download(&member.0.name, &self.name);
            let mut p = Payload::file(paths::blob_path(&member.0.name, &self.digest), size);
            p.content_type = content_type;
            with_digest(p, self.digest.clone())
        };
        Ok(Outcome::Found(payload))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Payload>> {
        let a = OciArtifact::Blob {
            name: upstream_name(up, &self.name),
            digest: self.digest.clone(),
        };
        Ok(match from_engine(cx, member, up, &a, self.head).await? {
            Outcome::Found(mut p) => {
                p.content_type
                    .get_or_insert_with(|| OCTET_STREAM.to_string());
                Outcome::Found(with_digest(p, self.digest.clone()))
            }
            Outcome::NotFound => Outcome::NotFound,
        })
    }
}

pub struct ManifestLeaf {
    pub name: String,
    pub reference: String,
    pub head: bool,
}

#[async_trait::async_trait]
impl Leaf for ManifestLeaf {
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Payload>> {
        let db = &cx.state.db;
        let Some(digest) =
            resolve_hosted_digest(db, member.0.id, &self.name, &self.reference).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let Some(manifest) =
            crate::db::oci::get_manifest(db, member.0.id, &self.name, &digest).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let size = manifest.size.max(0) as u64;
        let payload = if self.head {
            Payload::head_only(size, Some(manifest.content_type), Some(digest))
        } else {
            let image = format!("{}/{}", member.0.name, self.name);
            let mut p = Payload::file(paths::manifest_path(&image, &self.name, &digest), size);
            p.content_type = Some(manifest.content_type);
            with_digest(p, digest)
        };
        Ok(Outcome::Found(payload))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Payload>> {
        let name = upstream_name(up, &self.name);
        let a = if is_digest(&self.reference) {
            OciArtifact::Manifest {
                name,
                digest: self.reference.clone(),
            }
        } else {
            OciArtifact::Tag {
                name,
                tag: self.reference.clone(),
            }
        };
        let found = if self.head {
            from_engine(cx, member, up, &a, true).await?
        } else {
            let cached = fetch_cached(cx, member, up, &a).await?;
            if let Outcome::Found(c) = &cached {
                let version = Some(self.reference.clone());
                policy::record(cx, member, up, Format::Oci, &self.name, version, || {
                    Source::Oci {
                        body: c.clone(),
                        served: None,
                        parsed: None,
                    }
                });
            }
            cached.into_payload()
        };
        // Cache rows hold the bare body hash; the wire digest carries its algorithm.
        Ok(match found {
            Outcome::Found(mut p) => {
                p.digest = p.digest.map(|hex| format!("sha256:{hex}"));
                Outcome::Found(p)
            }
            Outcome::NotFound => Outcome::NotFound,
        })
    }
}

pub struct TagsLeaf {
    pub name: String,
}

/// A member is `Found` only when it holds at least one tag for the image,
/// so an image no member knows stays an empty listing without a false hit.
#[async_trait::async_trait]
impl Leaf for TagsLeaf {
    type Out = Vec<String>;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Vec<String>>> {
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM oci_tags WHERE repository_id = ?1 AND name = ?2 ORDER BY tag",
        )
        .bind(member.0.id)
        .bind(&self.name)
        .fetch_all(&cx.state.db)
        .await?;
        Ok(if tags.is_empty() {
            Outcome::NotFound
        } else {
            Outcome::Found(tags)
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Vec<String>>> {
        let a = OciArtifact::Tags {
            name: upstream_name(up, &self.name),
        };
        let engine = &cx.state.proxy;
        let Outcome::Found(cached) = engine.fetch(&OciUpstream, up, member, &a).await? else {
            return Ok(Outcome::NotFound);
        };
        let body: serde_json::Value = serde_json::from_slice(&engine.bytes(&cached).await?)
            .map_err(|e| AppError::BadGateway(format!("invalid tag list from upstream: {e}")))?;
        let tags = body["tags"]
            .as_array()
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Outcome::Found(tags))
    }
}
