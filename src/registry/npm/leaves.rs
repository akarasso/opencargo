use chrono::Utc;
use serde_json::Value;

use crate::app::search;
use crate::domain::{CacheRepo, Format, Outcome, Sighting};
use crate::policy::{self, Source};
use crate::ports::packages::NameMatch;
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::packument::{
    dist_tags_map, hosted_packument, strip_versions_to_abbreviated, Packument,
};
use super::search::search_in_repo;
use super::upstream::{NpmArtifact, NpmUpstream};

pub struct PackumentLeaf {
    pub name: String,
    pub abbreviated: bool,
}

#[async_trait::async_trait]
impl Leaf for PackumentLeaf {
    type Out = Packument;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Packument>, ResolveError> {
        let built = hosted_packument(cx.packages, member, &self.name, self.abbreviated).await?;
        Ok(match built {
            Outcome::Found(json) => Outcome::Found(Packument { json, stale: false }),
            Outcome::NotFound => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Packument>, ResolveError> {
        let artifact = NpmArtifact::Metadata {
            name: self.name.clone(),
        };
        let engine = cx.proxy;
        let Outcome::Found(cached) = engine.fetch(&NpmUpstream, up, member, &artifact).await? else {
            return Ok(Outcome::NotFound);
        };
        let bytes = engine.bytes(&cached).await?;
        let mut json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| ResolveError::Upstream(format!("invalid packument from upstream: {e}")))?;
        remember(cx, member, cached.exchanged, &json, &self.name).await;
        if self.abbreviated {
            strip_versions_to_abbreviated(&mut json);
        }
        Ok(Outcome::Found(Packument {
            json,
            stale: cached.stale,
        }))
    }
}

/// What a packument says about the package itself, so that a search answers
/// for it: npm is the one format whose document carries a description.
async fn remember(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    exchanged: bool,
    packument: &Value,
    name: &str,
) {
    let seen = Sighting {
        repository_id: member.0.id,
        format: Format::Npm,
        name,
        description: packument.get("description").and_then(Value::as_str),
        latest_version: packument
            .get("dist-tags")
            .and_then(|tags| tags.get("latest"))
            .and_then(Value::as_str),
    };
    search::remember(cx.cached, exchanged, &seen, Utc::now()).await;
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

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Value>, ResolveError> {
        let packument = PackumentLeaf {
            name: self.name.clone(),
            abbreviated: true,
        };
        Ok(match packument.proxy(cx, member, up).await? {
            Outcome::Found(mut p) => Outcome::Found(
                p.json
                    .get_mut("dist-tags")
                    .map(Value::take)
                    .unwrap_or_else(|| Value::Object(Default::default())),
            ),
            Outcome::NotFound => Outcome::NotFound,
        })
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
