//! One source per vendor. Every npm-shaped repository, whoever serves it,
//! is enumerated off its own npm endpoint, so one sink serves them all.

use chrono::{DateTime, Utc};
use reqwest::Url;
use serde_json::Value;

use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::domain::import::glob_match;
use crate::ports::import::{
    Coord, Digests, Item, Origin, PkgExtra, SourceError, SourceFilter, SourceFormat, VersionExtra,
};

pub mod artifactory;
pub mod nexus;
pub mod verdaccio;

/// `--source-repo` names or globs; none selects every repository.
pub fn repo_selected(f: &SourceFilter, name: &str) -> bool {
    f.source_repos.is_empty() || f.source_repos.iter().any(|g| glob_match(g, name))
}

/// A source's sparse cargo index, read for its lines' `cksum` and `yanked`.
pub struct CargoIndex<'a> {
    pub gate: &'a Gate,
    pub base: Url,
}

impl CargoIndex<'_> {
    /// The index file of `name`, `None` when the source serves no index.
    pub async fn lines(&self, name: &str) -> Result<Option<String>, SourceError> {
        let path = crate::adapters::import::sink::cargo::index_path(name);
        let url = self.base.join(&path).map_err(|e| SourceError::Refused(e.to_string()))?;
        match self.gate.bytes(&Req::get(url), 64 << 20).await {
            Ok(b) => Ok(Some(String::from_utf8_lossy(&b).to_string())),
            Err(FetchError::NotFound(_)) => Ok(None),
            Err(FetchError::Auth(m)) => Err(SourceError::Auth(m)),
            Err(e) => Err(e.into()),
        }
    }
}

/// Every version of a full packument, as items whose bytes are fetched off
/// `registry` again at copy time.
pub fn npm_items(registry: &Url, repo: &str, packument: &Value) -> Vec<Item> {
    let Some(name) = packument.get("name").and_then(|n| n.as_str()) else { return Vec::new() };
    let Some(versions) = packument.get("versions").and_then(|v| v.as_object()) else { return Vec::new() };
    let dist_tags: Vec<(String, String)> = packument
        .get("dist-tags")
        .and_then(|t| t.as_object())
        .map(|m| {
            let mut tags: Vec<(String, String)> =
                m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
            tags.sort();
            tags
        })
        .unwrap_or_default();
    let time = packument.get("time");
    let registry_s = crate::ports::import::redact(registry);
    let mut out: Vec<Item> = versions
        .iter()
        .map(|(v, meta)| {
            let s = |p: &str| meta.pointer(p).and_then(|x| x.as_str()).map(String::from);
            Item {
                source_ref: format!("npm:{registry_s}{name}@{v}"),
                format: SourceFormat::Npm,
                coord: Coord { repo: repo.to_string(), name: name.to_string(), version: v.clone() },
                published_at: time
                    .and_then(|t| t.get(v))
                    .and_then(|t| t.as_str())
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc)),
                size: None,
                want: Digests { sha1: s("/dist/shasum"), integrity: s("/dist/integrity"), ..Default::default() },
                origin: Origin::Npm { registry: registry_s.clone(), package: name.to_string() },
                pkg: PkgExtra { dist_tags: dist_tags.clone(), labels: Vec::new() },
                extra: VersionExtra::default(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.coord.version.cmp(&b.coord.version));
    out
}
