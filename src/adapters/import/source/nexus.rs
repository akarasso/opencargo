//! Nexus Repository 3: one stream per repository, walked through the
//! components API's continuation token. npm and cargo items are read off
//! the repository's own npm endpoint and sparse index; an asset keeps its
//! repository path, never its `downloadUrl`, so a pre-signed redirect is
//! always followed fresh.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::Value;

use super::{npm_items, repo_selected, CargoIndex};
use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::sink::npm_url;
use crate::domain::import::GapKind;
use crate::ports::import::{
    redact, Coord, Cursor, Digests, Discovered, Gap, Item, Origin, PkgExtra, Principal, Probe, Source, SourceError,
    SourceFilter, SourceFormat, VersionExtra,
};

pub struct Nexus {
    gate: Arc<Gate>,
}

impl Nexus {
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate }
    }

    fn url(&self, path: &str) -> Result<Url, SourceError> {
        self.gate.from().join(path).map_err(|e| SourceError::Refused(e.to_string()))
    }

    async fn repositories(&self) -> Result<Vec<Value>, SourceError> {
        let v = self.gate.get_json(self.url("service/rest/v1/repositories")?).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn repo_base(&self, repo: &str) -> Result<Url, SourceError> {
        self.url(&format!("repository/{repo}/"))
    }
}

pub fn source_format(nexus: &str) -> SourceFormat {
    match nexus {
        "npm" => SourceFormat::Npm,
        "docker" => SourceFormat::Oci,
        "go" => SourceFormat::Go,
        "cargo" => SourceFormat::Cargo,
        "maven2" => SourceFormat::Maven,
        "pypi" => SourceFormat::Pypi,
        "nuget" => SourceFormat::Nuget,
        "raw" => SourceFormat::Raw,
        "rubygems" => SourceFormat::RubyGems,
        "helm" => SourceFormat::Helm,
        _ => SourceFormat::Other,
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn digests(asset: &Value) -> Digests {
    let c = asset.get("checksum").cloned().unwrap_or(Value::Null);
    Digests { sha256: s(&c, "sha256"), sha1: s(&c, "sha1"), integrity: None, md5: s(&c, "md5") }
}

#[async_trait]
impl Source for Nexus {
    fn kind(&self) -> &'static str {
        "nexus"
    }

    async fn probe(&self) -> Result<Probe, SourceError> {
        let resp = self.gate.ok(&Req::get(self.url("service/rest/v1/status")?)).await?;
        let version = resp
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Nexus/"))
            .map(|v| v.split_whitespace().next().unwrap_or(v).to_string());
        self.repositories().await?;
        Ok(Probe { product: "nexus".into(), version, authenticated_as: None, capabilities: Vec::new() })
    }

    /// Users and their roles' repository-view privileges,
    /// `nx-repository-view-{format}-{repo}-{action}`; a role granting a
    /// role is followed, an external user (LDAP, SAML, ...) is not mapped.
    async fn principals(&self) -> Result<Vec<Principal>, SourceError> {
        let users = self.gate.get_json(self.url("service/rest/v1/security/users")?).await?;
        let roles = self.gate.get_json(self.url("service/rest/v1/security/roles")?).await?;
        let roles: BTreeMap<String, Value> = roles
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((s(r, "id")?, r.clone())))
            .collect();
        let repos: Vec<(String, String)> = self
            .repositories()
            .await?
            .iter()
            .filter_map(|r| Some((s(r, "name")?, s(r, "format")?)))
            .collect();
        let mut out = Vec::new();
        for u in users.as_array().into_iter().flatten() {
            let Some(name) = s(u, "userId") else { continue };
            let external = s(u, "source").is_some_and(|src| src != "default");
            let mut privileges = BTreeSet::new();
            let mut stack: Vec<String> = u
                .get("roles")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(String::from))
                .collect();
            let mut seen = BTreeSet::new();
            while let Some(role) = stack.pop() {
                if !seen.insert(role.clone()) {
                    continue;
                }
                let Some(r) = roles.get(&role) else { continue };
                for p in r.get("privileges").and_then(|p| p.as_array()).into_iter().flatten().filter_map(|p| p.as_str()) {
                    privileges.insert(p.to_string());
                }
                for sub in r.get("roles").and_then(|p| p.as_array()).into_iter().flatten().filter_map(|p| p.as_str()) {
                    stack.push(sub.to_string());
                }
            }
            for (repo, format) in &repos {
                let prefix = format!("nx-repository-view-{format}-{repo}-");
                let wild = format!("nx-repository-view-{format}-*-");
                let actions: Vec<&str> = privileges
                    .iter()
                    .filter_map(|p| p.strip_prefix(&prefix).or_else(|| p.strip_prefix(&wild)))
                    .collect();
                if actions.is_empty() {
                    continue;
                }
                let publish = actions.iter().any(|a| matches!(*a, "add" | "edit" | "*"));
                let read = publish || actions.iter().any(|a| matches!(*a, "read" | "browse"));
                out.push(Principal {
                    name: name.clone(),
                    kind: if external { "external user".into() } else { "user".into() },
                    repo: repo.clone(),
                    read,
                    publish,
                });
            }
        }
        Ok(out)
    }

    async fn streams(&self, f: &SourceFilter) -> Result<Vec<String>, SourceError> {
        let mut names: Vec<String> = self
            .repositories()
            .await?
            .iter()
            .filter_map(|r| s(r, "name"))
            .filter(|n| repo_selected(f, n))
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
        let Some(repo) = repos.iter().find(|r| s(r, "name").as_deref() == Some(stream)) else {
            out.gap(Gap::new(GapKind::ListingIncomplete, stream, "the repository disappeared from the source"));
            return Ok((None, true));
        };
        let kind = s(repo, "type").unwrap_or_default();
        let format = s(repo, "format").unwrap_or_default();
        if kind == "group" {
            out.gap(Gap::new(GapKind::SourceOnlyFeature, stream, "group repository: its members are imported on their own"));
            return Ok((None, true));
        }
        if kind == "proxy" && !f.include_proxy_caches {
            out.gap(Gap::new(
                GapKind::SourceOnlyFeature,
                stream,
                "proxy repository skipped (a cache of its upstream); --include-proxy-caches copies it",
            ));
            return Ok((None, true));
        }
        let sf = source_format(&format);
        if sf.target().is_none() {
            out.gap(Gap::new(
                GapKind::UnsupportedFormat,
                stream,
                format!("{format} repository: not a format this registry serves, nothing listed"),
            ));
            return Ok((None, true));
        }
        let mut url = self.url("service/rest/v1/components")?;
        url.query_pairs_mut().append_pair("repository", stream);
        if let Some(token) = &at {
            url.query_pairs_mut().append_pair("continuationToken", token);
        }
        let page = self.gate.get_json(url).await?;
        let components = page.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
        let next = s(&page, "continuationToken");
        let base = self.repo_base(stream)?;
        let base_s = redact(&base);
        match sf {
            SourceFormat::Npm => {
                let names: BTreeSet<String> = components
                    .iter()
                    .filter_map(|c| {
                        let name = s(c, "name")?;
                        Some(match s(c, "group").filter(|g| !g.is_empty()) {
                            Some(g) => format!("@{}/{name}", g.trim_start_matches('@')),
                            None => name,
                        })
                    })
                    .collect();
                for name in names {
                    match self.gate.json(&Req::get(npm_url(&base, &name)).header("accept", "application/json"), 64 << 20).await {
                        Ok(p) => npm_items(&base, stream, &p).into_iter().for_each(|it| out.item(it)),
                        Err(FetchError::Auth(m)) => return Err(SourceError::Auth(m)),
                        Err(e) => out.gap(Gap::new(GapKind::ListingIncomplete, format!("{stream}/{name}"), format!("packument unreadable: {e}"))),
                    }
                }
            }
            SourceFormat::Cargo => {
                let index = CargoIndex { gate: &self.gate, base: base.clone() };
                let mut by_crate: BTreeMap<String, Vec<(&Value, String)>> = BTreeMap::new();
                for c in &components {
                    if let (Some(n), Some(v)) = (s(c, "name"), s(c, "version")) {
                        by_crate.entry(n).or_default().push((c, v));
                    }
                }
                for (name, versions) in by_crate {
                    let lines = index.lines(&name).await?;
                    for (c, v) in versions {
                        let asset = c.get("assets").and_then(|a| a.get(0)).cloned().unwrap_or(Value::Null);
                        let line = lines.as_ref().and_then(|ls| crate::adapters::import::sink::cargo::find_line(ls, &v));
                        let (origin, want, yanked) = match &line {
                            Some(l) => (
                                Origin::Cargo { index: base_s.clone(), name: name.clone(), version: v.clone() },
                                Digests { sha256: s(l, "cksum"), ..Default::default() },
                                l.get("yanked").and_then(|y| y.as_bool()).unwrap_or(false),
                            ),
                            None => (
                                Origin::Asset {
                                    endpoint: redact(&self.url("repository/")?),
                                    repo: stream.to_string(),
                                    path: s(&asset, "path").unwrap_or_default().trim_start_matches('/').to_string(),
                                },
                                digests(&asset),
                                false,
                            ),
                        };
                        out.item(Item {
                            source_ref: format!("nexus:{stream}:{}", s(c, "id").unwrap_or_else(|| format!("{name}@{v}"))),
                            format: SourceFormat::Cargo,
                            coord: Coord { repo: stream.to_string(), name: name.clone(), version: v },
                            published_at: None,
                            size: asset.get("fileSize").and_then(|x| x.as_u64()),
                            want,
                            origin,
                            pkg: PkgExtra::default(),
                            extra: VersionExtra { yanked },
                        });
                    }
                }
            }
            SourceFormat::Go | SourceFormat::Oci => {
                for c in &components {
                    let (Some(name), Some(version)) = (s(c, "name"), s(c, "version")) else { continue };
                    let assets = c.get("assets").and_then(|a| a.as_array()).cloned().unwrap_or_default();
                    let zip = assets.iter().find(|a| s(a, "path").is_some_and(|p| p.ends_with(".zip")));
                    let (origin, want) = if sf == SourceFormat::Go {
                        (
                            Origin::Go { proxy: base_s.clone(), module: name.clone(), version: version.clone() },
                            zip.map(digests).unwrap_or_default(),
                        )
                    } else {
                        (Origin::Oci { registry: base_s.clone(), image: name.clone(), reference: version.clone() }, Digests::default())
                    };
                    out.item(Item {
                        source_ref: format!("nexus:{stream}:{}", s(c, "id").unwrap_or_else(|| format!("{name}@{version}"))),
                        format: sf,
                        coord: Coord { repo: stream.to_string(), name, version },
                        published_at: None,
                        size: zip.and_then(|a| a.get("fileSize")).and_then(|x| x.as_u64()),
                        want,
                        origin,
                        pkg: PkgExtra::default(),
                        extra: VersionExtra::default(),
                    });
                }
            }
            other => {
                for c in &components {
                    let (Some(name), Some(version)) = (s(c, "name"), s(c, "version")) else { continue };
                    let name = match s(c, "group").filter(|g| !g.is_empty()) {
                        Some(g) => format!("{g}:{name}"),
                        None => name,
                    };
                    let asset = c.get("assets").and_then(|a| a.get(0)).cloned().unwrap_or(Value::Null);
                    out.item(Item {
                        source_ref: format!("nexus:{stream}:{}", s(c, "id").unwrap_or_else(|| format!("{name}@{version}"))),
                        format: other,
                        coord: Coord { repo: stream.to_string(), name, version },
                        published_at: None,
                        size: asset.get("fileSize").and_then(|x| x.as_u64()),
                        want: digests(&asset),
                        origin: Origin::Asset {
                            endpoint: redact(&self.url("repository/")?),
                            repo: stream.to_string(),
                            path: s(&asset, "path").unwrap_or_default().trim_start_matches('/').to_string(),
                        },
                        pkg: PkgExtra::default(),
                        extra: VersionExtra::default(),
                    });
                }
            }
        }
        let done = next.is_none();
        Ok((next, done))
    }
}
