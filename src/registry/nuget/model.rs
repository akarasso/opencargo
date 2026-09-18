//! What a registration says about one version, without a single URL: the
//! renderer is the only writer of URLs, so nothing read from an upstream
//! can leak one into a served document except the catalog leaf it names.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Package, Version};

use super::nuspec::{DependencyGroup, Nuspec};
use super::version::NuGetVersion;

/// The facts a hosted version row carries in `metadata_json`: the filters
/// port 17 reads, and the nuspec as parsed and as pushed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostedFacts {
    pub prerelease: bool,
    pub semver2: bool,
    pub package_types: Vec<String>,
    pub nuspec: Nuspec,
    pub nuspec_xml: String,
}

impl HostedFacts {
    pub fn new(nuspec: Nuspec, nuspec_xml: String, version: &NuGetVersion) -> Self {
        Self {
            prerelease: version.is_prerelease(),
            semver2: version.is_semver2(),
            package_types: nuspec
                .package_types
                .iter()
                .map(|t| t.name.to_ascii_lowercase())
                .collect(),
            nuspec,
            nuspec_xml,
        }
    }

    pub fn of(version: &Version) -> Self {
        serde_json::from_str(&version.metadata_json).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The id as the package spells it.
    pub id: String,
    /// The normalized version with metadata and case, for display.
    pub version: String,
    /// The flat-container key.
    pub key: String,
    pub listed: bool,
    pub published: Option<DateTime<Utc>>,
    pub nuspec: Nuspec,
    /// An upstream catalog leaf: the one upstream URL a document may carry.
    pub catalog: Option<String>,
}

impl Entry {
    pub fn precedence(&self) -> Option<NuGetVersion> {
        NuGetVersion::parse(&self.version).ok()
    }

    pub fn from_hosted(package: &Package, version: &Version) -> Self {
        let facts = HostedFacts::of(version);
        let id = if facts.nuspec.id.is_empty() {
            package.name.clone()
        } else {
            facts.nuspec.id.clone()
        };
        let display = NuGetVersion::parse(&facts.nuspec.version)
            .map(|v| v.full())
            .unwrap_or_else(|_| version.version.clone());
        Self {
            id,
            version: display,
            key: version.version.clone(),
            listed: !version.yanked,
            published: Some(version.published_at),
            nuspec: facts.nuspec,
            catalog: None,
        }
    }

    /// A `catalogEntry` object of an upstream registration page; `None`
    /// when it lacks an id or a parseable version.
    pub fn from_catalog_entry(entry: &Value) -> Option<Self> {
        let s = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_string);
        let id = s("id")?;
        let version = NuGetVersion::parse(&s("version")?).ok()?;
        let published = s("published")
            .and_then(|p| DateTime::parse_from_rfc3339(&p).ok())
            .map(|d| d.with_timezone(&Utc));
        let listed = entry
            .get("listed")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| published.is_none_or(|p| p.format("%Y").to_string() != "1900"));
        let tags = match entry.get("tags") {
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Some(Value::String(t)) => Some(t.clone()),
            _ => None,
        };
        let groups = entry
            .get("dependencyGroups")
            .and_then(Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .map(|g| DependencyGroup {
                        target_framework: g
                            .get("targetFramework")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        dependencies: g
                            .get("dependencies")
                            .and_then(Value::as_array)
                            .map(|deps| {
                                deps.iter()
                                    .filter_map(|d| {
                                        Some(super::nuspec::Dependency {
                                            id: d.get("id")?.as_str()?.to_string(),
                                            range: d
                                                .get("range")
                                                .and_then(Value::as_str)
                                                .map(str::to_string),
                                            exclude: None,
                                            include: None,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let nuspec = Nuspec {
            id: id.clone(),
            version: version.full(),
            title: s("title"),
            description: s("description"),
            summary: s("summary"),
            authors: match entry.get("authors") {
                Some(Value::Array(a)) => Some(
                    a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
                ),
                Some(Value::String(t)) => Some(t.clone()),
                _ => None,
            },
            tags,
            project_url: s("projectUrl"),
            license_url: s("licenseUrl"),
            license_expression: s("licenseExpression"),
            icon_url: s("iconUrl"),
            require_license_acceptance: entry
                .get("requireLicenseAcceptance")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            dependency_groups: groups,
            ..Nuspec::default()
        };
        Some(Self {
            id,
            key: version.key(),
            version: version.full(),
            listed,
            published,
            nuspec,
            catalog: s("@id"),
        })
    }
}

/// Entries of several members, one per key, the first member's winning;
/// sorted by precedence.
pub fn merge(members: Vec<Vec<Entry>>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for entries in members {
        for e in entries {
            if !out.iter().any(|o| o.key == e.key) {
                out.push(e);
            }
        }
    }
    sort(&mut out);
    out
}

pub fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| super::version::compare_keys(&a.key, &b.key));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_catalog_entry_becomes_an_entry_without_its_urls() {
        let e = Entry::from_catalog_entry(&json!({
            "@id": "https://api.nuget.org/v3/catalog0/data/x.json",
            "id": "Newtonsoft.Json",
            "version": "13.0.1",
            "published": "2021-03-22T20:10:49.757+00:00",
            "packageContent": "https://api.nuget.org/v3-flatcontainer/newtonsoft.json/13.0.1/newtonsoft.json.13.0.1.nupkg",
            "authors": "James Newton-King",
            "tags": ["json"],
            "dependencyGroups": [{"targetFramework": ".NETStandard2.0", "dependencies": [{"id": "A", "range": "[1.0.0, )"}]}]
        }))
        .unwrap();
        assert_eq!(e.key, "13.0.1");
        assert!(e.listed);
        assert_eq!(e.nuspec.dependency_groups[0].dependencies[0].id, "A");
        assert_eq!(e.catalog.as_deref(), Some("https://api.nuget.org/v3/catalog0/data/x.json"));
        let unlisted =
            Entry::from_catalog_entry(&json!({"id": "a", "version": "1.0", "published": "1900-01-01T00:00:00+00:00"}))
                .unwrap();
        assert!(!unlisted.listed);
        assert_eq!(unlisted.key, "1.0.0");
        assert!(Entry::from_catalog_entry(&json!({"id": "a", "version": "nope"})).is_none());
    }

    #[test]
    fn merge_keeps_the_first_member_and_sorts_by_precedence() {
        let e = |key: &str, id: &str| Entry {
            id: id.into(),
            version: key.into(),
            key: key.into(),
            listed: true,
            published: None,
            nuspec: Nuspec::default(),
            catalog: None,
        };
        let merged = merge(vec![
            vec![e("1.10.0", "first"), e("1.0.0", "first")],
            vec![e("1.9.0", "second"), e("1.0.0", "second")],
        ]);
        let keys: Vec<&str> = merged.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["1.0.0", "1.9.0", "1.10.0"]);
        assert_eq!(merged[0].id, "first");
    }
}
