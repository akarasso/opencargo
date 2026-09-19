//! Artifactory: one stream per local repository, walked by AQL offset. The
//! AQL rows name files only; npm and cargo metadata come from the
//! repository's own npm and cargo APIs, never from a listing row.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Method, Url};
use serde_json::Value;

use super::{npm_items, repo_selected, CargoIndex};
use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::sink::npm_url;
use crate::domain::import::GapKind;
use crate::ports::import::{
    redact, Coord, Cursor, Digests, Discovered, Gap, Item, Origin, PkgExtra, Principal, Probe, Source, SourceError,
    SourceFilter, SourceFormat, VersionExtra,
};
use crate::registry::go::escape::unescape;

/// Artifactory refuses an AQL body past this many characters.
pub const AQL_MAX: usize = 6000;
const PAGE: usize = 1000;

pub struct Artifactory {
    gate: Arc<Gate>,
    page: usize,
}

impl Artifactory {
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate, page: PAGE }
    }

    pub fn with_page(mut self, page: usize) -> Self {
        self.page = page.max(1);
        self
    }

    fn url(&self, path: &str) -> Result<Url, SourceError> {
        self.gate.from().join(path).map_err(|e| SourceError::Refused(e.to_string()))
    }

    async fn repositories(&self) -> Result<Vec<Value>, SourceError> {
        let v = self.gate.get_json(self.url("api/repositories")?).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}

pub fn source_format(package_type: &str) -> SourceFormat {
    match package_type.to_ascii_lowercase().as_str() {
        "npm" => SourceFormat::Npm,
        "docker" | "oci" => SourceFormat::Oci,
        "go" => SourceFormat::Go,
        "cargo" => SourceFormat::Cargo,
        "maven" | "gradle" => SourceFormat::Maven,
        "pypi" => SourceFormat::Pypi,
        "nuget" => SourceFormat::Nuget,
        "generic" => SourceFormat::Raw,
        "gems" => SourceFormat::RubyGems,
        "helm" => SourceFormat::Helm,
        _ => SourceFormat::Other,
    }
}

pub fn aql(repo: &str, offset: usize, limit: usize) -> Result<String, SourceError> {
    let repo = serde_json::to_string(repo).map_err(|e| SourceError::Refused(e.to_string()))?;
    let body = format!(
        r#"items.find({{"repo":{{"$eq":{repo}}}}}).include("repo","path","name","actual_sha1","sha256","size","modified").sort({{"$asc":["path","name"]}}).offset({offset}).limit({limit})"#
    );
    if body.len() > AQL_MAX {
        return Err(SourceError::Refused(format!(
            "the AQL query for this repository is {} characters, over Artifactory's {AQL_MAX}",
            body.len()
        )));
    }
    Ok(body)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn row_digests(row: &Value) -> Digests {
    Digests { sha256: s(row, "sha256"), sha1: s(row, "actual_sha1"), ..Default::default() }
}

#[async_trait]
impl Source for Artifactory {
    fn kind(&self) -> &'static str {
        "artifactory"
    }

    async fn probe(&self) -> Result<Probe, SourceError> {
        let v = self.gate.get_json(self.url("api/system/version")?).await?;
        Ok(Probe {
            product: "artifactory".into(),
            version: s(&v, "version"),
            authenticated_as: None,
            capabilities: Vec::new(),
        })
    }

    /// Permission targets: a user's actions on each repository a target
    /// names; groups and targets over every repository are not mapped.
    async fn principals(&self) -> Result<Vec<Principal>, SourceError> {
        let list = self.gate.get_json(self.url("api/v2/security/permissions")?).await?;
        let mut out = Vec::new();
        for t in list.as_array().into_iter().flatten() {
            let Some(name) = s(t, "name") else { continue };
            let target = self.gate.get_json(self.url(&format!("api/v2/security/permissions/{name}"))?).await?;
            let Some(repo) = target.get("repo") else { continue };
            let repos: Vec<String> = repo
                .get("repositories")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(String::from))
                .collect();
            for (kind, key) in [("user", "users"), ("group", "groups")] {
                let Some(who) = repo.pointer(&format!("/actions/{key}")).and_then(|w| w.as_object()) else { continue };
                for (principal, actions) in who {
                    let acts: Vec<&str> = actions.as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
                    let publish = acts.iter().any(|a| matches!(*a, "write" | "deploy" | "manage"));
                    let read = publish || acts.contains(&"read");
                    for r in &repos {
                        let kind = if r.starts_with("ANY") { "rule over every repository" } else { kind };
                        out.push(Principal { name: principal.clone(), kind: kind.into(), repo: r.clone(), read, publish });
                    }
                }
            }
        }
        Ok(out)
    }

    async fn streams(&self, f: &SourceFilter) -> Result<Vec<String>, SourceError> {
        let mut names: Vec<String> = self
            .repositories()
            .await?
            .iter()
            .filter_map(|r| s(r, "key"))
            .filter(|k| repo_selected(f, k))
            .collect();
        names.sort();
        Ok(names)
    }

    async fn discover(
        &self,
        f: &SourceFilter,
        stream: &str,
        at: Cursor,
        out: &mut dyn Discovered,
    ) -> Result<(Cursor, bool), SourceError> {
        let repos = self.repositories().await?;
        let Some(repo) = repos.iter().find(|r| s(r, "key").as_deref() == Some(stream)) else {
            out.gap(Gap::new(GapKind::ListingIncomplete, stream, "the repository disappeared from the source"));
            return Ok((None, true));
        };
        let kind = s(repo, "type").unwrap_or_default().to_ascii_uppercase();
        let package_type = s(repo, "packageType").unwrap_or_default();
        if kind == "VIRTUAL" {
            out.gap(Gap::new(GapKind::SourceOnlyFeature, stream, "virtual repository: its members are imported on their own"));
            return Ok((None, true));
        }
        if kind == "REMOTE" && !f.include_proxy_caches {
            out.gap(Gap::new(
                GapKind::SourceOnlyFeature,
                stream,
                "remote repository skipped (a cache of its upstream); --include-proxy-caches copies it",
            ));
            return Ok((None, true));
        }
        let sf = source_format(&package_type);
        if sf.target().is_none() {
            out.gap(Gap::new(
                GapKind::UnsupportedFormat,
                stream,
                format!("{package_type} repository: not a format this registry serves, nothing listed"),
            ));
            return Ok((None, true));
        }
        let offset: usize = at.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
        let body = aql(stream, offset, self.page)?;
        let req = Req::get(self.url("api/search/aql")?).header("content-type", "text/plain").body(Method::POST, body);
        let doc = self.gate.json(&req, 256 << 20).await?;
        let rows = doc.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        let assets = redact(&self.url("")?);
        let item = |name: String, version: String, format: SourceFormat, origin: Origin, row: &Value, extra| Item {
            source_ref: format!("artifactory:{stream}:{}/{}", s(row, "path").unwrap_or_default(), s(row, "name").unwrap_or_default()),
            format,
            coord: Coord { repo: stream.to_string(), name, version },
            published_at: s(row, "modified")
                .and_then(|m| chrono::DateTime::parse_from_rfc3339(&m).ok())
                .map(|t| t.with_timezone(&chrono::Utc)),
            size: row.get("size").and_then(|x| x.as_u64()),
            want: row_digests(row),
            origin,
            pkg: PkgExtra::default(),
            extra,
        };
        match sf {
            SourceFormat::Npm => {
                let registry = self.url(&format!("api/npm/{stream}/"))?;
                let mut by_pkg: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
                for r in rows.iter().filter(|r| s(r, "name").is_some_and(|n| n.ends_with(".tgz"))) {
                    if let Some((p, _)) = s(r, "path").as_deref().and_then(|p| p.split_once("/-")) {
                        by_pkg.entry(p.to_string()).or_default().push(r);
                    }
                }
                for (name, files) in by_pkg {
                    match self.gate.json(&Req::get(npm_url(&registry, &name)).header("accept", "application/json"), 64 << 20).await {
                        Ok(p) => npm_items(&registry, stream, &p).into_iter().for_each(|it| out.item(it)),
                        Err(FetchError::Auth(m)) => return Err(SourceError::Auth(m)),
                        Err(FetchError::NotFound(_)) => {
                            let short = name.split_once('/').map_or(name.as_str(), |(_, n)| n).to_string();
                            for r in files {
                                let file = s(r, "name").unwrap_or_default();
                                let Some(version) = file.strip_prefix(&format!("{short}-")).and_then(|v| v.strip_suffix(".tgz")) else {
                                    continue;
                                };
                                let origin = Origin::Npm { registry: redact(&registry), package: name.clone() };
                                let mut it = item(name.clone(), version.to_string(), SourceFormat::Npm, origin, r, VersionExtra::default());
                                it.source_ref = format!("npm:{}{name}@{version}", redact(&registry));
                                out.item(it);
                            }
                        }
                        Err(e) => out.gap(Gap::new(GapKind::ListingIncomplete, format!("{stream}/{name}"), format!("packument unreadable: {e}"))),
                    }
                }
            }
            SourceFormat::Cargo => {
                let base = self.url(&format!("api/cargo/{stream}/index/"))?;
                let index = CargoIndex { gate: &self.gate, base: base.clone() };
                let mut by_crate: BTreeMap<String, Vec<(&Value, String)>> = BTreeMap::new();
                for r in &rows {
                    let (Some(path), Some(file)) = (s(r, "path"), s(r, "name")) else { continue };
                    let Some(name) = path.rsplit('/').next().map(String::from) else { continue };
                    let Some(version) = file.strip_prefix(&format!("{name}-")).and_then(|v| v.strip_suffix(".crate")) else { continue };
                    by_crate.entry(name).or_default().push((r, version.to_string()));
                }
                for (name, versions) in by_crate {
                    let lines = index.lines(&name).await?;
                    for (row, v) in versions {
                        let line = lines.as_ref().and_then(|ls| crate::adapters::import::sink::cargo::find_line(ls, &v));
                        let yanked = line.as_ref().and_then(|l| l.get("yanked")).and_then(|y| y.as_bool()).unwrap_or(false);
                        let origin = match line {
                            Some(_) => Origin::Cargo { index: redact(&base), name: name.clone(), version: v.clone() },
                            None => Origin::Asset {
                                endpoint: assets.clone(),
                                repo: stream.to_string(),
                                path: format!("{}/{}", s(row, "path").unwrap_or_default(), s(row, "name").unwrap_or_default()),
                            },
                        };
                        out.item(item(name.clone(), v, SourceFormat::Cargo, origin, row, VersionExtra { yanked }));
                    }
                }
            }
            SourceFormat::Go => {
                let proxy = redact(&self.url(&format!("api/go/{stream}/"))?);
                for r in &rows {
                    let (Some(path), Some(file)) = (s(r, "path"), s(r, "name")) else { continue };
                    let (Some(module), Some(version)) = (path.strip_suffix("/@v"), file.strip_suffix(".zip")) else { continue };
                    let (module, version) = (unescape(module), unescape(version));
                    let origin = Origin::Go { proxy: proxy.clone(), module: module.clone(), version: version.clone() };
                    out.item(item(module, version, SourceFormat::Go, origin, r, VersionExtra::default()));
                }
            }
            SourceFormat::Oci => {
                let registry = redact(&self.url(&format!("api/docker/{stream}/"))?);
                for r in &rows {
                    let (Some(path), Some(file)) = (s(r, "path"), s(r, "name")) else { continue };
                    if file != "manifest.json" && file != "list.manifest.json" {
                        continue;
                    }
                    let Some((image, tag)) = path.rsplit_once('/') else { continue };
                    let origin = Origin::Oci { registry: registry.clone(), image: image.to_string(), reference: tag.to_string() };
                    let mut it = item(image.to_string(), tag.to_string(), SourceFormat::Oci, origin, r, VersionExtra::default());
                    it.want = Digests::default();
                    out.item(it);
                }
            }
            other => {
                for r in &rows {
                    let (Some(path), Some(file)) = (s(r, "path"), s(r, "name")) else { continue };
                    let origin = Origin::Asset { endpoint: assets.clone(), repo: stream.to_string(), path: format!("{path}/{file}") };
                    out.item(item(path.clone(), file.clone(), other, origin, r, VersionExtra::default()));
                }
            }
        }
        let done = rows.len() < self.page;
        Ok((Some((offset + rows.len()).to_string()), done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aql_body_over_6000_chars_is_refused_before_sending() {
        assert!(aql("libs-release", 0, 1000).unwrap().contains(r#""$eq":"libs-release""#));
        assert!(aql(&"x".repeat(AQL_MAX), 0, 1000).is_err());
    }
}
