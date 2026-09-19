use bytes::Bytes;
use serde_json::Value;

use crate::domain::{CacheRepo, Format, Outcome};
use crate::policy::{self, Source};
use crate::ports::packages::NameMatch;
use crate::proxy::strategy::CacheKey;
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::packument::{dist_tags_map, hosted_packument};
use super::render::{self, Flavor};
use super::search::search_in_repo;
use super::upstream::{NpmArtifact, NpmUpstream};

/// The packument a client is served: already rendered, so nothing above
/// this leaf holds the document as a tree.
pub struct PackumentLeaf {
    pub name: String,
    pub abbreviated: bool,
}

impl PackumentLeaf {
    fn flavor(&self) -> Flavor {
        Flavor::of(self.abbreviated)
    }

    fn served(&self, body: Vec<u8>) -> Payload {
        let mut payload = Payload::bytes(Bytes::from(body));
        payload.content_type = Some(self.flavor().content_type().to_string());
        payload
    }

    /// What a rendering is remembered under: everything it depends on, the
    /// document it came from included. A packument that changes upstream,
    /// or a server that moves, lands under a new key, so a rendering is
    /// never invalidated -- only left behind for the sweep.
    fn rendering(&self, cx: &Cx<'_>, source: Option<&str>) -> Option<CacheKey> {
        Some(CacheKey {
            kind: "npm-packument-rendered",
            key: format!(
                "{}|{}|{}|{}|{}",
                cx.base_url,
                cx.url.0,
                self.name,
                self.flavor().tag(),
                source?
            ),
        })
    }
}

#[async_trait::async_trait]
impl Leaf for PackumentLeaf {
    type Out = Payload;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let built = hosted_packument(cx.packages, member, &self.name, self.abbreviated).await?;
        let Outcome::Found(mut json) = built else {
            return Ok(Outcome::NotFound);
        };
        render::rewrite_tarball_urls(&mut json, cx.base_url, cx.url.0, &self.name);
        let body = serde_json::to_vec(&json)
            .map_err(|e| ResolveError::Internal(format!("packument is not serializable: {e}")))?;
        Ok(Outcome::Found(self.served(body)))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let artifact = NpmArtifact::Metadata {
            name: self.name.clone(),
        };
        let engine = cx.proxy;
        let Outcome::Found(cached) = engine.fetch(&NpmUpstream, up, member, &artifact).await? else {
            return Ok(Outcome::NotFound);
        };
        let key = self.rendering(cx, cached.entry.digest.as_deref());
        if let Some(key) = &key {
            if let Some(mut payload) = engine.derived(member, key).await? {
                payload.stale = cached.stale;
                return Ok(Outcome::Found(payload));
            }
        }
        let raw = engine.bytes(&cached).await?;
        let body = render::render(&raw, self.flavor(), cx.base_url, cx.url.0, &self.name)
            .map_err(|e| ResolveError::Upstream(format!("invalid packument from upstream: {e}")))?;
        drop(raw);
        let content_type = self.flavor().content_type();
        let mut payload = match &key {
            Some(key) => {
                engine
                    .put_derived(member, key, Bytes::from(body), content_type)
                    .await?
            }
            None => self.served(body),
        };
        payload.stale = cached.stale;
        Ok(Outcome::Found(payload))
    }
}

pub struct TarballLeaf {
    pub name: String,
    pub filename: String,
}

#[async_trait::async_trait]
impl Leaf for TarballLeaf {
    type Out = Payload;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let found = cx
            .packages
            .package(member.0.id, &self.name, NameMatch::Exact)
            .await?;
        let Some(package) = found else {
            return Ok(Outcome::NotFound);
        };
        let versions = cx.packages.versions(package.id).await?;
        let Some(version) = versions
            .iter()
            .find(|v| crate::domain::layout::logical_key(&v.tarball_path).ends_with(&self.filename))
        else {
            return Ok(Outcome::NotFound);
        };
        let _ = cx.packages.record_download(version.id).await;
        crate::telemetry::record_download(&member.0.name, &self.name);
        Ok(Outcome::Found(Payload::file(
            version.tarball_path.clone(),
            version.size.max(0) as u64,
        )))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let artifact = NpmArtifact::Tarball {
            name: self.name.clone(),
            filename: self.filename.clone(),
        };
        let cached = cx.proxy.fetch(&NpmUpstream, up, member, &artifact).await?;
        if let Outcome::Found(c) = &cached {
            policy::record(cx, member, up, Format::Npm, &self.name, None, || Source::Npm {
                filename: self.filename.clone(),
                digest: c.entry.digest.clone(),
            });
        }
        Ok(cached.into_payload())
    }
}

/// The `dist-tags` object of a package: hosted from the `dist_tags` rows,
/// proxied from the cached packument.
pub struct DistTagsLeaf {
    pub name: String,
}

#[async_trait::async_trait]
impl Leaf for DistTagsLeaf {
    type Out = Value;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Value>, ResolveError> {
        let packages = cx.packages;
        let found = packages
            .package(member.0.id, &self.name, NameMatch::Exact)
            .await?;
        let Some(package) = found else {
            return Ok(Outcome::NotFound);
        };
        let versions = packages.versions(package.id).await?;
        let tags = dist_tags_map(packages, package.id, &versions).await?;
        let json = serde_json::to_value(tags)
            .map_err(|e| ResolveError::Internal(format!("dist-tags are not serializable: {e}")))?;
        Ok(Outcome::Found(json))
    }

    /// One field of the cached packument, never the document as a tree:
    /// a tag list is a handful of strings whatever the packument weighs.
    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Value>, ResolveError> {
        let artifact = NpmArtifact::Metadata {
            name: self.name.clone(),
        };
        let engine = cx.proxy;
        let Outcome::Found(cached) = engine.fetch(&NpmUpstream, up, member, &artifact).await? else {
            return Ok(Outcome::NotFound);
        };
        let raw = engine.bytes(&cached).await?;
        let tags = render::field(&raw, "dist-tags")
            .map_err(|e| ResolveError::Upstream(format!("invalid packument from upstream: {e}")))?;
        Ok(Outcome::Found(
            tags.unwrap_or_else(|| Value::Object(Default::default())),
        ))
    }
}

/// The first `limit` local search objects of a member; a proxy member has no
/// searchable index and contributes nothing.
pub struct SearchLeaf {
    pub text: String,
    pub limit: i64,
}

#[async_trait::async_trait]
impl Leaf for SearchLeaf {
    type Out = Vec<Value>;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Vec<Value>>, ResolveError> {
        let objects =
            search_in_repo(cx.search, cx.packages, member.0.id, &self.text, self.limit).await?;
        Ok(Outcome::Found(objects))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        _member: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<Vec<Value>>, ResolveError> {
        Ok(Outcome::NotFound)
    }
}
