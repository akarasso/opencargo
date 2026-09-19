use bytes::Bytes;
use chrono::Utc;
use serde_json::Value;

use crate::app::search;
use crate::domain::{CacheRepo, Format, Outcome, Package, Sighting, Version};
use crate::policy::{self, Source};
use crate::ports::packages::NameMatch;
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::escape::unescape;
use super::upstream::{FileKind, GoArtifact, GoUpstream};
use super::{info_json, latest_of};

/// Rows keep the raw module path; the escaped form only travels upstream.
async fn hosted_package(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    module: &str,
) -> Result<Option<Package>, ResolveError> {
    let found = cx
        .packages
        .package(member.0.id, &unescape(module), NameMatch::Exact)
        .await?;
    Ok(found)
}

async fn hosted_versions(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    module: &str,
) -> Result<Option<Vec<Version>>, ResolveError> {
    let Some(package) = hosted_package(cx, member, module).await? else {
        return Ok(None);
    };
    Ok(Some(cx.packages.versions(package.id).await?))
}

async fn fetch_bytes(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &GoArtifact,
) -> Result<Outcome<Bytes>, ResolveError> {
    let engine = cx.proxy;
    Ok(match engine.fetch(&GoUpstream, up, member, a).await? {
        Outcome::Found(cached) => {
            let bytes = engine.bytes(&cached).await?;
            remember(cx, member, cached.exchanged, a, &bytes).await;
            Outcome::Found(bytes)
        }
        Outcome::NotFound => Outcome::NotFound,
    })
}

/// A module proxy serves no description, and only the two documents that are
/// about the module as a whole say which version is newest: a file fetch names
/// a version the client asked for, which is not that.
async fn remember(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    exchanged: bool,
    a: &GoArtifact,
    body: &Bytes,
) {
    let module = match a {
        GoArtifact::List { module } | GoArtifact::Latest { module } => module,
        GoArtifact::File { module, .. } => module,
    };
    let latest = match a {
        GoArtifact::List { .. } => String::from_utf8_lossy(body)
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map(String::from),
        GoArtifact::Latest { .. } => serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|doc| doc.get("Version").and_then(Value::as_str).map(String::from)),
        GoArtifact::File { .. } => None,
    };
    let name = unescape(module);
    let seen = Sighting {
        repository_id: member.0.id,
        format: Format::Go,
        name: &name,
        description: None,
        latest_version: latest.as_deref(),
    };
    search::remember(cx.cached, exchanged, &seen, Utc::now()).await;
}

/// `Found` only when the member knows the module, even with no versions.
pub struct ListLeaf {
    pub module: String,
}

#[async_trait::async_trait]
impl Leaf for ListLeaf {
    type Out = Vec<String>;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Vec<String>>, ResolveError> {
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
    ) -> Result<Outcome<Vec<String>>, ResolveError> {
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

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Value>, ResolveError> {
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
    ) -> Result<Outcome<Value>, ResolveError> {
        let artifact = GoArtifact::Latest {
            module: self.module.clone(),
        };
        Ok(match fetch_bytes(cx, member, up, &artifact).await? {
            Outcome::Found(bytes) => {
                Outcome::Found(serde_json::from_slice(&bytes).map_err(|e| {
                    ResolveError::Upstream(format!("invalid @latest document from upstream: {e}"))
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

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let Some(package) = hosted_package(cx, member, &self.module).await? else {
            return Ok(Outcome::NotFound);
        };
        let found = cx
            .packages
            .version(package.id, &unescape(&self.version))
            .await?;
        let Some(version) = found else {
            return Ok(Outcome::NotFound);
        };
        let payload = match self.kind {
            FileKind::Info => Payload::bytes(Bytes::from(info_json(&version).to_string())),
            FileKind::Mod => Payload::bytes(Bytes::from(version.metadata_json)),
            FileKind::Zip => {
                let _ = cx.packages.record_download(version.id).await;
                crate::telemetry::record_download(&member.0.name, &package.name);
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
    ) -> Result<Outcome<Payload>, ResolveError> {
        let artifact = GoArtifact::File {
            module: self.module.clone(),
            version: self.version.clone(),
            kind: self.kind,
        };
        let cached = cx.proxy.fetch(&GoUpstream, up, member, &artifact).await?;
        if let (Outcome::Found(c), FileKind::Zip) = (&cached, self.kind) {
            let version = Some(unescape(&self.version));
            policy::record(cx, member, up, Format::Go, &unescape(&self.module), version, || {
                Source::Go {
                    digest: c.entry.digest.clone(),
                }
            });
        }
        Ok(cached.into_payload())
    }
}
