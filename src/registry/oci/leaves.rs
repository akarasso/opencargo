use crate::domain::{CacheRepo, Format, Outcome};
use crate::policy::{self, Source};
use crate::proxy::engine::{Cached, IntoPayload};
use crate::proxy::Payload;
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::manifests::resolve_hosted_digest;
use super::upstream::{upstream_name, OciArtifact, OciUpstream};
use super::is_digest;

const OCTET_STREAM: &str = "application/octet-stream";

/// One engine call per artifact: a HEAD is answered from the row or the
/// upstream headers, a GET lands the body first.
async fn from_engine(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &OciArtifact,
    head: bool,
) -> Result<Outcome<Payload>, ResolveError> {
    let engine = cx.proxy;
    if head {
        Ok(engine.head(&OciUpstream, up, member, a).await?)
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
) -> Result<Outcome<Cached>, ResolveError> {
    Ok(cx.proxy.fetch(&OciUpstream, up, member, a).await?)
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

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let Some(blob) = cx.oci.blob(member.0.id, &self.digest).await? else {
            return Ok(Outcome::NotFound);
        };
        let size = blob.size.max(0) as u64;
        let content_type = blob.content_type.or_else(|| Some(OCTET_STREAM.to_string()));
        let payload = if self.head {
            Payload::head_only(size, content_type, Some(self.digest.clone()))
        } else {
            crate::telemetry::record_download(&member.0.name, &self.name);
            let mut p = Payload::file(blob.key, size);
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
    ) -> Result<Outcome<Payload>, ResolveError> {
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

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let Some(digest) =
            resolve_hosted_digest(cx.oci, member.0.id, &self.name, &self.reference).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let Some(manifest) = cx.oci.manifest(member.0.id, &self.name, &digest).await? else {
            return Ok(Outcome::NotFound);
        };
        let size = manifest.size.max(0) as u64;
        let payload = if self.head {
            Payload::head_only(size, Some(manifest.content_type), Some(digest))
        } else {
            let mut p = Payload::file(manifest.key, size);
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
    ) -> Result<Outcome<Payload>, ResolveError> {
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

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Vec<String>>, ResolveError> {
        let tags = cx.oci.tags(member.0.id, &self.name).await?;
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
    ) -> Result<Outcome<Vec<String>>, ResolveError> {
        let a = OciArtifact::Tags {
            name: upstream_name(up, &self.name),
        };
        let engine = cx.proxy;
        let Outcome::Found(cached) = engine.fetch(&OciUpstream, up, member, &a).await? else {
            return Ok(Outcome::NotFound);
        };
        let body: serde_json::Value = serde_json::from_slice(&engine.bytes(&cached).await?)
            .map_err(|e| ResolveError::Upstream(format!("invalid tag list from upstream: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Format, RepoKind, RepoSpec, Repository, Visibility};
    use crate::proxy::engine::Src;
    use crate::testing::fakes::FakeDb;
    use crate::testing::resolver::Resolver;

    const DIGEST: &str = "sha256:abc";

    async fn hosted_repo(db: &FakeDb) -> Repository {
        db.repositories()
            .create(
                &RepoSpec {
                    name: "oci-hosted",
                    kind: RepoKind::Hosted,
                    format: Format::Oci,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                },
                chrono::DateTime::UNIX_EPOCH,
            )
            .await
            .unwrap()
    }

    /// A HEAD answers from the row alone, and a blob with no recorded media
    /// type is served as opaque bytes rather than as nothing.
    #[tokio::test]
    async fn a_hosted_blob_comes_from_the_store_or_is_a_miss() {
        let fx = Resolver::default();
        let repo = hosted_repo(&fx.fakes).await;
        fx.fakes.add_blob(repo.id, DIGEST, 12, None);
        let cx = fx.cx(None, &repo.name);

        let leaf = BlobLeaf {
            name: "app".into(),
            digest: DIGEST.into(),
            head: true,
        };
        let Outcome::Found(payload) = leaf.hosted(&cx, CacheRepo(&repo)).await.unwrap() else {
            panic!("the seeded blob is there");
        };
        assert_eq!(payload.size, 12);
        assert_eq!(payload.content_type.as_deref(), Some(OCTET_STREAM));
        assert!(matches!(payload.src, Src::HeadOnly));

        let absent = BlobLeaf {
            name: "app".into(),
            digest: "sha256:nope".into(),
            head: true,
        };
        assert!(matches!(
            absent.hosted(&cx, CacheRepo(&repo)).await.unwrap(),
            Outcome::NotFound
        ));
    }

    /// A tag is resolved to a digest through the store; a digest reference is
    /// its own answer and never reaches it.
    #[tokio::test]
    async fn a_hosted_manifest_is_reached_by_tag_and_by_digest() {
        let fx = Resolver::default();
        let repo = hosted_repo(&fx.fakes).await;
        fx.fakes.add_manifest(repo.id, "app", DIGEST, "application/json", 7);
        fx.fakes.add_tag(repo.id, "app", "v1", DIGEST);
        let cx = fx.cx(None, &repo.name);

        for reference in ["v1", DIGEST] {
            let leaf = ManifestLeaf {
                name: "app".into(),
                reference: reference.into(),
                head: true,
            };
            let Outcome::Found(payload) = leaf.hosted(&cx, CacheRepo(&repo)).await.unwrap() else {
                panic!("{reference}: the seeded manifest is there");
            };
            assert_eq!(payload.size, 7);
            assert_eq!(payload.digest.as_deref(), Some(DIGEST));
        }

        let unknown = ManifestLeaf {
            name: "app".into(),
            reference: "v2".into(),
            head: true,
        };
        assert!(matches!(
            unknown.hosted(&cx, CacheRepo(&repo)).await.unwrap(),
            Outcome::NotFound
        ));
    }

    /// An image the member holds no tag for is a miss, not an empty hit: an
    /// empty `Found` would stop a group walk at the first member.
    #[tokio::test]
    async fn a_member_without_tags_does_not_answer_the_listing() {
        let fx = Resolver::default();
        let repo = hosted_repo(&fx.fakes).await;
        fx.fakes.add_tag(repo.id, "app", "v2", DIGEST);
        fx.fakes.add_tag(repo.id, "app", "v1", DIGEST);
        fx.fakes.add_tag(repo.id, "other", "v1", DIGEST);
        let cx = fx.cx(None, &repo.name);

        let leaf = TagsLeaf { name: "app".into() };
        let Outcome::Found(tags) = leaf.hosted(&cx, CacheRepo(&repo)).await.unwrap() else {
            panic!("the image has tags");
        };
        assert_eq!(tags, vec!["v1", "v2"], "in tag order");

        let empty = TagsLeaf {
            name: "absent".into(),
        };
        assert!(matches!(
            empty.hosted(&cx, CacheRepo(&repo)).await.unwrap(),
            Outcome::NotFound
        ));
    }
}
