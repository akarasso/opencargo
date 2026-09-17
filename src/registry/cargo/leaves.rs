use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::proxy::{Payload, ProxyEngine};
use crate::registry::resolve::{CacheRepo, Cx, Leaf, Outcome, Upstream};

use super::line_field;
use super::upstream::{CargoArtifact, CargoUpstream};

/// One member's index lines for a crate, verbatim from a proxy member.
pub struct IndexLines {
    pub lines: Vec<String>,
    pub stale: bool,
}

pub struct IndexLeaf {
    pub name: String,
}

#[async_trait::async_trait]
impl Leaf for IndexLeaf {
    type Out = IndexLines;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<IndexLines>> {
        let db = &cx.state.db;
        let Some(package) = crate::db::get_package(db, member.0.id, &self.name).await? else {
            return Ok(Outcome::NotFound);
        };
        let versions = crate::db::get_versions(db, package.id).await?;
        if versions.is_empty() {
            return Ok(Outcome::NotFound);
        }
        let lines = versions
            .iter()
            .map(|v| build_index_line(&self.name, v))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Outcome::Found(IndexLines {
            lines,
            stale: false,
        }))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<IndexLines>> {
        fetch_index_lines(&cx.state.proxy, up, member, &self.name).await
    }
}

pub struct CrateLeaf {
    pub name: String,
    pub version: String,
}

#[async_trait::async_trait]
impl Leaf for CrateLeaf {
    type Out = Payload;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Payload>> {
        let db = &cx.state.db;
        let Some(package) = crate::db::get_package(db, member.0.id, &self.name).await? else {
            return Ok(Outcome::NotFound);
        };
        let Some(version) = crate::db::get_version(db, package.id, &self.version).await? else {
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
        let engine = &cx.state.proxy;
        let dl = fetch_dl_template(engine, up, member).await?;
        let Outcome::Found(index) = fetch_index_lines(engine, up, member, &self.name).await? else {
            return Ok(Outcome::NotFound);
        };
        let Some(cksum) = checksum_of(&index.lines, &self.version) else {
            return Ok(Outcome::NotFound);
        };
        let artifact = CargoArtifact::Crate {
            name: self.name.clone(),
            version: self.version.clone(),
            dl,
            cksum,
        };
        let cached = engine.fetch(&CargoUpstream, up, member, &artifact).await?;
        Ok(cached.into_payload())
    }
}

async fn fetch_index_lines(
    engine: &ProxyEngine,
    up: &Upstream,
    member: CacheRepo<'_>,
    name: &str,
) -> AppResult<Outcome<IndexLines>> {
    let artifact = CargoArtifact::Index {
        name: name.to_string(),
    };
    let Outcome::Found(cached) = engine.fetch(&CargoUpstream, up, member, &artifact).await? else {
        return Ok(Outcome::NotFound);
    };
    let bytes = engine.bytes(&cached).await?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| AppError::BadGateway(format!("invalid index from upstream: {e}")))?;
    let lines = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    Ok(Outcome::Found(IndexLines {
        lines,
        stale: cached.stale,
    }))
}

/// The upstream's `dl` template; an index without `config.json` is broken.
async fn fetch_dl_template(
    engine: &ProxyEngine,
    up: &Upstream,
    member: CacheRepo<'_>,
) -> AppResult<String> {
    let Outcome::Found(cached) = engine
        .fetch(&CargoUpstream, up, member, &CargoArtifact::Config)
        .await?
    else {
        return Err(AppError::BadGateway(
            "upstream index has no config.json".into(),
        ));
    };
    let bytes = engine.bytes(&cached).await?;
    let config: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::BadGateway(format!("invalid config.json from upstream: {e}")))?;
    config
        .get("dl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| AppError::BadGateway("upstream config.json has no dl".into()))
}

fn checksum_of(lines: &[String], version: &str) -> Option<String> {
    lines
        .iter()
        .find(|l| line_field(l, "vers").as_deref() == Some(version))
        .and_then(|l| line_field(l, "cksum"))
}

fn build_index_line(crate_name: &str, version: &crate::db::Version) -> AppResult<String> {
    let meta: Value = serde_json::from_str(&version.metadata_json).unwrap_or(json!({}));
    let mut line = json!({
        "name": crate_name,
        "vers": version.version,
        "deps": meta.get("deps").cloned().unwrap_or(json!([])),
        "cksum": version.checksum_sha256.clone().unwrap_or_default(),
        "features": meta.get("features").cloned().unwrap_or(json!({})),
        "yanked": version.yanked != 0,
    });
    for key in ["features2", "links"] {
        if let Some(v) = meta.get(key) {
            line[key] = v.clone();
        }
    }
    Ok(serde_json::to_string(&line)?)
}
