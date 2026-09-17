use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::proxy::Payload;
use crate::registry::resolve::{CacheRepo, Cx, Leaf, Outcome, Upstream};

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

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Packument>> {
        let built = hosted_packument(&cx.state.db, member, &self.name, self.abbreviated).await?;
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
    ) -> AppResult<Outcome<Packument>> {
        let artifact = NpmArtifact::Metadata {
            name: self.name.clone(),
        };
        let engine = &cx.state.proxy;
        let Outcome::Found(cached) = engine.fetch(&NpmUpstream, up, member, &artifact).await? else {
            return Ok(Outcome::NotFound);
        };
        let bytes = engine.bytes(&cached).await?;
        let mut json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| AppError::BadGateway(format!("invalid packument from upstream: {e}")))?;
        if self.abbreviated {
            strip_versions_to_abbreviated(&mut json);
        }
        Ok(Outcome::Found(Packument {
            json,
            stale: cached.stale,
        }))
    }
}

pub struct TarballLeaf {
    pub name: String,
    pub filename: String,
}

#[async_trait::async_trait]
impl Leaf for TarballLeaf {
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Payload>> {
        let db = &cx.state.db;
        let Some(package) = crate::db::get_package(db, member.0.id, &self.name).await? else {
            return Ok(Outcome::NotFound);
        };
        let versions = crate::db::get_versions(db, package.id).await?;
        let Some(version) = versions
            .iter()
            .find(|v| v.tarball_path.ends_with(&self.filename))
        else {
            return Ok(Outcome::NotFound);
        };
        let _ = crate::db::record_download(db, version.id).await;
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
    ) -> AppResult<Outcome<Payload>> {
        let artifact = NpmArtifact::Tarball {
            name: self.name.clone(),
            filename: self.filename.clone(),
        };
        let cached = cx.state.proxy.fetch(&NpmUpstream, up, member, &artifact).await?;
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

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Value>> {
        let db = &cx.state.db;
        let Some(package) = crate::db::get_package(db, member.0.id, &self.name).await? else {
            return Ok(Outcome::NotFound);
        };
        let versions = crate::db::get_versions(db, package.id).await?;
        let tags = dist_tags_map(db, package.id, &versions).await?;
        Ok(Outcome::Found(serde_json::to_value(tags)?))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Value>> {
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

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Vec<Value>>> {
        let objects = search_in_repo(cx.state, member.0.id, &self.text, self.limit).await?;
        Ok(Outcome::Found(objects))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        _member: CacheRepo<'_>,
        _up: &Upstream,
    ) -> AppResult<Outcome<Vec<Value>>> {
        Ok(Outcome::NotFound)
    }
}
