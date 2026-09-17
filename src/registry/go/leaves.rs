use bytes::Bytes;
use serde_json::Value;

use crate::db::{Package, Version};
use crate::error::{AppError, AppResult};
use crate::proxy::Payload;
use crate::registry::resolve::{CacheRepo, Cx, Leaf, Outcome, Upstream};

use super::escape::unescape;
use super::upstream::{FileKind, GoArtifact, GoUpstream};
use super::{info_json, latest_of};

/// Rows keep the raw module path; the escaped form only travels upstream.
async fn hosted_package(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    module: &str,
) -> AppResult<Option<Package>> {
    Ok(crate::db::get_package(&cx.state.db, member.0.id, &unescape(module)).await?)
}

async fn hosted_versions(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    module: &str,
) -> AppResult<Option<Vec<Version>>> {
    let Some(package) = hosted_package(cx, member, module).await? else {
        return Ok(None);
    };
    Ok(Some(
        crate::db::get_versions(&cx.state.db, package.id).await?,
    ))
}

async fn fetch_bytes(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &GoArtifact,
) -> AppResult<Outcome<Bytes>> {
    let engine = &cx.state.proxy;
    Ok(match engine.fetch(&GoUpstream, up, member, a).await? {
        Outcome::Found(cached) => Outcome::Found(engine.bytes(&cached).await?),
        Outcome::NotFound => Outcome::NotFound,
    })
}

/// `Found` only when the member knows the module, even with no versions.
pub struct ListLeaf {
    pub module: String,
}

#[async_trait::async_trait]
impl Leaf for ListLeaf {
    type Out = Vec<String>;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Vec<String>>> {
        Ok(match hosted_versions(cx, member, &self.module).await? {
            Some(versions) => Outcome::Found(versions.into_iter().map(|v| v.version).collect()),
            None => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Vec<String>>> {
        let artifact = GoArtifact::List {
            module: self.module.clone(),
        };
        Ok(match fetch_bytes(cx, member, up, &artifact).await? {
            Outcome::Found(bytes) => Outcome::Found(
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect(),
            ),
            Outcome::NotFound => Outcome::NotFound,
        })
    }
}

/// One `{ "Version", "Time" }` document per member; the handler keeps the max.
pub struct LatestLeaf {
    pub module: String,
}

#[async_trait::async_trait]
impl Leaf for LatestLeaf {
    type Out = Value;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Value>> {
        let versions = hosted_versions(cx, member, &self.module).await?;
        Ok(match versions.as_deref().and_then(latest_of) {
            Some(latest) => Outcome::Found(info_json(latest)),
            None => Outcome::NotFound,
        })
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Value>> {
        let artifact = GoArtifact::Latest {
            module: self.module.clone(),
        };
        Ok(match fetch_bytes(cx, member, up, &artifact).await? {
            Outcome::Found(bytes) => {
                Outcome::Found(serde_json::from_slice(&bytes).map_err(|e| {
                    AppError::BadGateway(format!("invalid @latest document from upstream: {e}"))
                })?)
            }
            Outcome::NotFound => Outcome::NotFound,
        })
    }
}

pub struct FileLeaf {
    pub module: String,
    pub version: String,
    pub kind: FileKind,
}

#[async_trait::async_trait]
impl Leaf for FileLeaf {
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Payload>> {
        let db = &cx.state.db;
        let Some(package) = hosted_package(cx, member, &self.module).await? else {
            return Ok(Outcome::NotFound);
        };
        let Some(version) =
            crate::db::get_version(db, package.id, &unescape(&self.version)).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let payload = match self.kind {
            FileKind::Info => Payload::bytes(Bytes::from(info_json(&version).to_string())),
            FileKind::Mod => Payload::bytes(Bytes::from(version.metadata_json)),
            FileKind::Zip => {
                let _ = crate::db::record_download(db, version.id).await;
                Payload::file(version.tarball_path, version.size.max(0) as u64)
            }
        };
        Ok(Outcome::Found(payload))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Payload>> {
        let artifact = GoArtifact::File {
            module: self.module.clone(),
            version: self.version.clone(),
            kind: self.kind,
        };
        let cached = cx
            .state
            .proxy
            .fetch(&GoUpstream, up, member, &artifact)
            .await?;
        Ok(cached.into_payload())
    }
}
