use serde_json::{json, Value};

use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::policy::{self, Source};
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

    /// Cargo lowercases the index path; the line keeps the published case.
    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<IndexLines>> {
        let db = &cx.state.db;
        let Some(package) = crate::db::get_package_nocase(db, member.0.id, &self.name).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let versions = crate::db::get_versions(db, package.id).await?;
        if versions.is_empty() {
            return Ok(Outcome::NotFound);
        }
        let lines = versions
            .iter()
            .map(|v| build_index_line(&package.name, v))
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
        let Some(package) = crate::db::get_package_nocase(db, member.0.id, &self.name).await?
        else {
            return Ok(Outcome::NotFound);
        };
        let Some(version) = crate::db::get_version(db, package.id, &self.version).await? else {
            return Ok(Outcome::NotFound);
        };
        let _ = crate::db::record_download(db, version.id).await;
        crate::telemetry::record_download(&member.0.name, &package.name);
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
            cksum: cksum.clone(),
        };
        let cached = engine.fetch(&CargoUpstream, up, member, &artifact).await?;
        if let Outcome::Found(_) = &cached {
            policy::record(cx, member, up, Format::Cargo, &self.name, Some(self.version.clone()), || {
                Source::Cargo { cksum }
            });
        }
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

fn build_index_line(crate_name: &str, version: &crate::domain::Version) -> AppResult<String> {
    let meta: Value = serde_json::from_str(&version.metadata_json).unwrap_or(json!({}));
    let deps: Vec<Value> = meta
        .get("deps")
        .and_then(Value::as_array)
        .map(|deps| deps.iter().map(index_dep).collect())
        .unwrap_or_default();
    let mut line = json!({
        "name": crate_name,
        "vers": version.version,
        "deps": deps,
        "cksum": version.checksum_sha256.clone().unwrap_or_default(),
        "features": meta.get("features").cloned().unwrap_or(json!({})),
        "yanked": version.yanked,
    });
    for key in ["features2", "links"] {
        if let Some(v) = meta.get(key) {
            line[key] = v.clone();
        }
    }
    Ok(serde_json::to_string(&line)?)
}

/// The publish payload names the package and its alias as `name` /
/// `explicit_name_in_toml` with a `version_req`; the index wants `name` to
/// be the alias, `package` the real name and `req` the requirement.
fn index_dep(dep: &Value) -> Value {
    let package = dep.get("name").cloned().unwrap_or(Value::Null);
    let mut out = match dep.get("explicit_name_in_toml").filter(|a| a.is_string()) {
        Some(alias) => json!({ "name": alias, "package": package }),
        None => json!({ "name": package }),
    };
    out["req"] = dep.get("version_req").cloned().unwrap_or(json!("*"));
    for key in [
        "features",
        "optional",
        "default_features",
        "target",
        "kind",
        "registry",
    ] {
        if let Some(v) = dep.get(key).filter(|v| !v.is_null()) {
            out[key] = v.clone();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(metadata_json: &str) -> crate::domain::Version {
        crate::domain::Version {
            id: 1,
            package_id: 1,
            version: "0.1.0".into(),
            metadata_json: metadata_json.into(),
            checksum_sha1: None,
            checksum_sha256: Some("ab".repeat(32)),
            integrity: None,
            size: 1,
            tarball_path: String::new(),
            published_at: chrono::DateTime::UNIX_EPOCH,
            yanked: false,
        }
    }

    #[test]
    fn index_line_uses_the_sparse_index_dependency_shape() {
        let meta = r#"{"name":"App","vers":"0.1.0","deps":[
            {"name":"serde","version_req":"^1.0","features":["derive"],"optional":false,
             "default_features":true,"target":null,"kind":"normal","registry":null,
             "explicit_name_in_toml":null},
            {"name":"tokio","version_req":"1","kind":"dev","explicit_name_in_toml":"tk",
             "registry":"https://github.com/rust-lang/crates.io-index"}
        ],"features":{"x":[]},"links":"z"}"#;
        let line: Value = serde_json::from_str(&build_index_line("App", &version(meta)).unwrap())
            .unwrap();
        assert_eq!(line["name"], "App");
        assert_eq!(
            line["deps"][0],
            json!({"name":"serde","req":"^1.0","features":["derive"],"optional":false,
                   "default_features":true,"kind":"normal"})
        );
        assert_eq!(
            line["deps"][1],
            json!({"name":"tk","package":"tokio","req":"1","kind":"dev",
                   "registry":"https://github.com/rust-lang/crates.io-index"})
        );
        assert_eq!(line["links"], "z");
        assert!(line["deps"][0].get("version_req").is_none());

        let bare: Value =
            serde_json::from_str(&build_index_line("a", &version("{}")).unwrap()).unwrap();
        assert_eq!(bare["deps"], json!([]));
        assert_eq!(bare["features"], json!({}));
    }
}
