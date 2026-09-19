use bytes::Bytes;

use crate::domain::{CacheRepo, Outcome, Package, Version};
use crate::ports::packages::NameMatch;
use crate::proxy::Payload;
use crate::registry::resolve::{Cx, Leaf, ResolveError, Subject, Upstream};

use super::model::{self, Entry, HostedFacts};

/// The id and version of a request, already normalized: the lowercase id
/// and the flat-container key.
#[derive(Debug, Clone)]
pub struct Coordinates {
    pub id: String,
    pub key: Option<String>,
}

async fn hosted_package(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    id: &str,
) -> Result<Option<Package>, ResolveError> {
    Ok(cx.packages.package(member.0.id, id, NameMatch::Exact).await?)
}

/// The version stamp of `id` in a hosted member, `-` when the member has
/// no such package: what a memoized document derived from it is valid for.
pub async fn hosted_stamp(cx: &Cx<'_>, member: CacheRepo<'_>, id: &str) -> Result<String, ResolveError> {
    Ok(match hosted_package(cx, member, id).await? {
        Some(package) => cx.packages.stamp(package.id).await?,
        None => "-".to_string(),
    })
}

async fn hosted_version(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    at: &Coordinates,
) -> Result<Option<(Package, Version)>, ResolveError> {
    let (Some(package), Some(key)) = (hosted_package(cx, member, &at.id).await?, &at.key) else {
        return Ok(None);
    };
    Ok(cx.packages.version(package.id, key).await?.map(|v| (package, v)))
}

async fn hosted_entries(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    id: &str,
) -> Result<Outcome<Vec<Entry>>, ResolveError> {
    let Some(package) = hosted_package(cx, member, id).await? else {
        return Ok(Outcome::NotFound);
    };
    let versions = cx.packages.versions(package.id).await?;
    if versions.is_empty() {
        return Ok(Outcome::NotFound);
    }
    let mut entries: Vec<Entry> = versions.iter().map(|v| Entry::from_hosted(&package, v)).collect();
    model::sort(&mut entries);
    Ok(Outcome::Found(entries))
}

/// Every version a member knows of one id, listed or not: the flat index
/// and the registration are both rendered from it.
pub struct EntriesLeaf {
    pub id: String,
}

#[async_trait::async_trait]
impl Leaf for EntriesLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::of(&self.id)
    }
    type Out = Vec<Entry>;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Vec<Entry>>, ResolveError> {
        hosted_entries(cx, member, &self.id).await
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Vec<Entry>>, ResolveError> {
        super::upstream::entries(cx, member, up, &self.id).await
    }
}

pub struct NupkgLeaf {
    pub at: Coordinates,
}

#[async_trait::async_trait]
impl Leaf for NupkgLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::of(&self.at.id)
    }
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Payload>, ResolveError> {
        let Some((package, version)) = hosted_version(cx, member, &self.at).await? else {
            return Ok(Outcome::NotFound);
        };
        let _ = cx.packages.record_download(version.id).await;
        crate::telemetry::record_download(&member.0.name, &package.name);
        Ok(Outcome::Found(Payload::file(version.tarball_path, version.size.max(0) as u64)))
    }

    async fn proxy(&self, cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> Result<Outcome<Payload>, ResolveError> {
        super::upstream::nupkg(cx, member, up, &self.at).await
    }
}

pub struct NuspecLeaf {
    pub at: Coordinates,
}

#[async_trait::async_trait]
impl Leaf for NuspecLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::of(&self.at.id)
    }
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Payload>, ResolveError> {
        let Some((_, version)) = hosted_version(cx, member, &self.at).await? else {
            return Ok(Outcome::NotFound);
        };
        let xml = HostedFacts::of(&version).nuspec_xml;
        Ok(Outcome::Found(Payload::bytes(Bytes::from(xml))))
    }

    async fn proxy(&self, cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> Result<Outcome<Payload>, ResolveError> {
        super::upstream::nuspec(cx, member, up, &self.at).await
    }
}
