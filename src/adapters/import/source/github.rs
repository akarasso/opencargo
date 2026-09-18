//! GitHub Packages: one stream per owner and package type, listed through
//! the REST API; npm bytes from the npm registry, containers from the
//! container registry, both endpoints named here and credentialed like the
//! API. The API refuses `per_page * page` past 10 000, which ends a listing
//! as incomplete, and says so.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::Value;

use super::distribution::oci_item;
use super::npm_items;
use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::sink::npm_url;
use crate::domain::import::GapKind;
use crate::ports::import::{
    redact, Cursor, Discovered, Gap, Probe, Source, SourceError, SourceFilter,
};

pub const API_WINDOW: u32 = 10_000;
const PER_PAGE: u32 = 100;
const TYPES: [&str; 5] = ["npm", "container", "maven", "nuget", "rubygems"];

pub struct Github {
    gate: Arc<Gate>,
    npm: Url,
    registry: Url,
    per_page: u32,
}

impl Github {
    /// `npm` and `registry` default to npm.pkg.github.com and ghcr.io; the
    /// credential goes to both, and to nothing else the API names.
    pub fn new(gate: Arc<Gate>, npm: Url, registry: Url) -> Self {
        gate.name_endpoint(npm.clone());
        gate.name_endpoint(registry.clone());
        Self { gate, npm, registry, per_page: PER_PAGE }
    }

    pub fn with_page(mut self, per_page: u32) -> Self {
        self.per_page = per_page.clamp(1, PER_PAGE);
        self
    }

    fn api(&self, path: &str) -> Result<Url, SourceError> {
        self.gate.from().join(path).map_err(|e| SourceError::Refused(e.to_string()))
    }

    /// `/orgs/{owner}/...`, or `/users/{owner}/...` for a personal account.
    async fn list(&self, owner: &str, rest: &str) -> Result<Value, SourceError> {
        for scope in ["orgs", "users"] {
            match self.gate.get_json(self.api(&format!("{scope}/{owner}/{rest}"))?).await {
                Ok(v) => return Ok(v),
                Err(FetchError::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Value::Array(Vec::new()))
    }
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
        .unwrap_or_default()
}

fn encode(name: &str) -> String {
    name.replace('/', "%2F")
}

#[async_trait]
impl Source for Github {
    fn kind(&self) -> &'static str {
        "github"
    }

    async fn probe(&self) -> Result<Probe, SourceError> {
        let user = match self.gate.get_json(self.api("user")?).await {
            Ok(u) => u.get("login").and_then(|l| l.as_str()).map(String::from),
            Err(FetchError::Auth(m)) => return Err(SourceError::Auth(m)),
            Err(_) => None,
        };
        Ok(Probe { product: "github".into(), version: None, authenticated_as: user, capabilities: Vec::new() })
    }

    async fn streams(&self, f: &SourceFilter) -> Result<Vec<String>, SourceError> {
        if f.source_repos.is_empty() {
            return Err(SourceError::Refused("name the owners to import with --source-repo ORG".into()));
        }
        Ok(f.source_repos.iter().flat_map(|o| TYPES.iter().map(move |t| format!("{o}/{t}"))).collect())
    }

    async fn discover(
        &self,
        _f: &SourceFilter,
        stream: &str,
        at: Cursor,
        out: &mut dyn Discovered,
    ) -> Result<(Cursor, bool), SourceError> {
        let Some((owner, kind)) = stream.rsplit_once('/') else { return Ok((None, true)) };
        let page: u32 = at.as_deref().and_then(|c| c.parse().ok()).unwrap_or(1);
        if self.per_page * page > API_WINDOW {
            out.gap(Gap::new(
                GapKind::ListingIncomplete,
                stream,
                format!("the API lists no further than {API_WINDOW} packages (per_page x page)"),
            ));
            return Ok((at, true));
        }
        let listed = self.list(owner, &format!("packages?package_type={kind}&per_page={}&page={page}", self.per_page)).await?;
        let packages = names(&listed);
        match kind {
            "npm" => {
                for name in &packages {
                    let scoped = if name.starts_with('@') { name.clone() } else { format!("@{owner}/{name}") };
                    match self.gate.json(&Req::get(npm_url(&self.npm, &scoped)).header("accept", "application/json"), 64 << 20).await {
                        Ok(p) => npm_items(&self.npm, owner, &p).into_iter().for_each(|it| out.item(it)),
                        Err(FetchError::Auth(m)) => return Err(SourceError::Auth(m)),
                        Err(e) => out.gap(Gap::new(GapKind::ListingIncomplete, format!("{owner}/{name}"), format!("packument unreadable: {e}"))),
                    }
                }
            }
            "container" => {
                let registry = redact(&self.registry);
                for name in &packages {
                    let mut vpage = 1;
                    loop {
                        if self.per_page * vpage > API_WINDOW {
                            out.gap(Gap::new(
                                GapKind::ListingIncomplete,
                                format!("{owner}/{name}"),
                                format!("the API lists no further than {API_WINDOW} versions"),
                            ));
                            break;
                        }
                        let versions = self
                            .list(owner, &format!("packages/container/{}/versions?per_page={}&page={vpage}", encode(name), self.per_page))
                            .await?;
                        let list = versions.as_array().cloned().unwrap_or_default();
                        for v in &list {
                            let digest = v.get("name").and_then(|n| n.as_str()).map(String::from);
                            let tags = v.pointer("/metadata/container/tags").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                            for tag in tags.iter().filter_map(|t| t.as_str()) {
                                out.item(oci_item(&registry, &format!("{owner}/{name}"), tag, digest.clone()));
                            }
                        }
                        if (list.len() as u32) < self.per_page {
                            break;
                        }
                        vpage += 1;
                    }
                }
            }
            other => {
                if !packages.is_empty() {
                    let why = if other == "rubygems" { "not a format this registry serves" } else { "the importer has no sink for it yet" };
                    out.gap(Gap::new(
                        GapKind::UnsupportedFormat,
                        stream,
                        format!("{} {other} package(s) on this page: {why}", packages.len()),
                    ));
                }
            }
        }
        let done = (packages.len() as u32) < self.per_page;
        Ok((Some((page + 1).to_string()), done))
    }
}
