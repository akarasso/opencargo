//! One source per vendor. Every npm-shaped repository, whoever serves it,
//! is enumerated off its own npm endpoint, so one sink serves them all.

use chrono::{DateTime, Utc};
use reqwest::Url;
use serde_json::Value;

use crate::ports::import::{Coord, Digests, Item, Origin, PkgExtra, SourceFormat, VersionExtra};

pub mod verdaccio;

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
