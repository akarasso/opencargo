use chrono::Utc;
use serde_json::{json, Value};

use crate::app::search;
use crate::domain::{CacheRepo, Format, Outcome, Sighting};
use crate::policy::{self, Source};
use crate::ports::packages::NameMatch;
use crate::proxy::{IntoPayload, Payload, ProxyEngine};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Subject, Upstream};

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

    fn subject(&self) -> Subject<'_> {
        Subject::of(&self.name)
    }
    type Out = IndexLines;

    /// Cargo lowercases the index path; the line keeps the published case.
    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<IndexLines>, ResolveError> {
        let found = cx
            .packages
            .package(member.0.id, &self.name, NameMatch::Insensitive)
            .await?;
        let Some(package) = found else {
            return Ok(Outcome::NotFound);
        };
        let versions = cx.packages.versions(package.id).await?;
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
    ) -> Result<Outcome<IndexLines>, ResolveError> {
        fetch_index_lines(cx, up, member, &self.name).await
    }
}

pub struct CrateLeaf {
    pub name: String,
    pub version: String,
}

#[async_trait::async_trait]
impl Leaf for CrateLeaf {

    fn subject(&self) -> Subject<'_> {
        Subject::of(&self.name)
    }
    type Out = Payload;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let found = cx
            .packages
            .package(member.0.id, &self.name, NameMatch::Insensitive)
            .await?;
        let Some(package) = found else {
            return Ok(Outcome::NotFound);
        };
        let Some(version) = cx.packages.version(package.id, &self.version).await? else {
            return Ok(Outcome::NotFound);
        };
        let _ = cx.packages.record_download(version.id).await;
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
    ) -> Result<Outcome<Payload>, ResolveError> {
        let engine = cx.proxy;
        let dl = fetch_dl_template(engine, up, member).await?;
        let Outcome::Found(index) = fetch_index_lines(cx, up, member, &self.name).await? else {
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
    cx: &Cx<'_>,
    up: &Upstream,
    member: CacheRepo<'_>,
    name: &str,
) -> Result<Outcome<IndexLines>, ResolveError> {
    let engine = cx.proxy;
    let artifact = CargoArtifact::Index {
        name: name.to_string(),
    };
    let Outcome::Found(cached) = engine.fetch(&CargoUpstream, up, member, &artifact).await? else {
        return Ok(Outcome::NotFound);
    };
    let bytes = engine.bytes(&cached).await?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| ResolveError::Upstream(format!("invalid index from upstream: {e}")))?;
    let lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    remember(cx, member, cached.exchanged, &lines, name).await;
    Ok(Outcome::Found(IndexLines {
        lines,
        stale: cached.stale,
    }))
}

/// A crate's index carries no description, and its newest version is its last
/// line -- the order crates.io publishes in.
async fn remember(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    exchanged: bool,
    lines: &[String],
    name: &str,
) {
    let latest = lines.last().and_then(|line| line_field(line, "vers"));
    let seen = Sighting {
        repository_id: member.0.id,
        format: Format::Cargo,
        name,
        description: None,
        latest_version: latest.as_deref(),
    };
    search::remember(cx.cached, exchanged, &seen, Utc::now()).await;
}

/// The upstream's `dl` template; an index without `config.json` is broken.
async fn fetch_dl_template(
    engine: &ProxyEngine,
    up: &Upstream,
    member: CacheRepo<'_>,
) -> Result<String, ResolveError> {
    let Outcome::Found(cached) = engine
        .fetch(&CargoUpstream, up, member, &CargoArtifact::Config)
        .await?
    else {
        return Err(ResolveError::Upstream(
            "upstream index has no config.json".into(),
        ));
    };
    let bytes = engine.bytes(&cached).await?;
    let config: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ResolveError::Upstream(format!("invalid config.json from upstream: {e}")))?;
    config
        .get("dl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ResolveError::Upstream("upstream config.json has no dl".into()))
}

fn checksum_of(lines: &[String], version: &str) -> Option<String> {
    lines
        .iter()
        .find(|l| line_field(l, "vers").as_deref() == Some(version))
        .and_then(|l| line_field(l, "cksum"))
}

fn build_index_line(
    crate_name: &str,
    version: &crate::domain::Version,
) -> Result<String, ResolveError> {
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
    for key in ["v", "features2", "links", "rust_version"] {
        if let Some(v) = meta.get(key) {
            line[key] = v.clone();
        }
    }
    serde_json::to_string(&line)
        .map_err(|e| ResolveError::Internal(format!("index line is not serializable: {e}")))
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
