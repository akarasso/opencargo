//! The only writer of URLs in a NuGet document. Every URL is built from
//! this server's base and the repository the client asked for, segment by
//! segment; the catalog leaf of an upstream entry is the one exception.

use serde_json::{json, Value};

use super::model::Entry;

/// Leaves per registration page.
pub const PAGE_SIZE: usize = 64;
/// Up to this many versions, pages are inlined in the index.
pub const INLINE_MAX: usize = 128;

const UNLISTED_PUBLISHED: &str = "1900-01-01T00:00:00+00:00";

/// `{base_url}/{repo}/v3`, the root every URL of one response hangs off.
pub struct Base {
    root: String,
    hosted: bool,
}

fn segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

impl Base {
    pub fn new(base_url: &str, repo: &str, hosted: bool) -> Self {
        Self {
            root: format!("{}/{}/v3", base_url.trim_end_matches('/'), segment(repo)),
            hosted,
        }
    }

    fn id(id: &str) -> String {
        segment(&id.to_ascii_lowercase())
    }

    pub fn registration_index(&self, id: &str) -> String {
        format!("{}/registration/{}/index.json", self.root, Self::id(id))
    }

    pub fn registration_leaf(&self, id: &str, key: &str) -> String {
        format!("{}/registration/{}/{}.json", self.root, Self::id(id), segment(key))
    }

    pub fn registration_page(&self, id: &str, lower: &str, upper: &str) -> String {
        format!(
            "{}/registration/{}/page/{}/{}.json",
            self.root,
            Self::id(id),
            segment(lower),
            segment(upper)
        )
    }

    pub fn package_content(&self, id: &str, key: &str) -> String {
        let id = Self::id(id);
        let key = segment(key);
        format!("{}/flatcontainer/{id}/{key}/{id}.{key}.nupkg", self.root)
    }

    pub fn service_index(&self) -> Value {
        let mut resources = vec![
            json!({"@id": format!("{}/flatcontainer/", self.root), "@type": "PackageBaseAddress/3.0.0"}),
        ];
        for t in ["RegistrationsBaseUrl", "RegistrationsBaseUrl/3.4.0", "RegistrationsBaseUrl/3.6.0"] {
            resources.push(json!({"@id": format!("{}/registration/", self.root), "@type": t}));
        }
        for t in [
            "SearchQueryService",
            "SearchQueryService/3.0.0-beta",
            "SearchQueryService/3.0.0-rc",
            "SearchQueryService/3.5.0",
        ] {
            resources.push(json!({"@id": format!("{}/search", self.root), "@type": t}));
        }
        if self.hosted {
            resources.push(json!({"@id": format!("{}/package", self.root), "@type": "PackagePublish/2.0.0"}));
        }
        json!({"version": "3.0.0", "resources": resources})
    }

    fn catalog_entry(&self, e: &Entry) -> Value {
        let n = &e.nuspec;
        let groups: Vec<Value> = n
            .dependency_groups
            .iter()
            .map(|g| {
                let deps: Vec<Value> = g
                    .dependencies
                    .iter()
                    .map(|d| json!({"id": d.id, "range": d.range.clone().unwrap_or_default()}))
                    .collect();
                let mut group = json!({"dependencies": deps});
                if let Some(tf) = &g.target_framework {
                    group["targetFramework"] = json!(tf);
                }
                group
            })
            .collect();
        let tags: Vec<&str> = n
            .tags
            .as_deref()
            .unwrap_or("")
            .split([' ', ','])
            .filter(|t| !t.is_empty())
            .collect();
        json!({
            "@id": e.catalog.clone().unwrap_or_else(|| format!("{}#catalog", self.registration_leaf(&e.id, &e.key))),
            "@type": "PackageDetails",
            "id": e.id,
            "version": e.version,
            "authors": n.authors.clone().unwrap_or_default(),
            "description": n.description.clone().unwrap_or_default(),
            "summary": n.summary.clone().unwrap_or_default(),
            "title": n.title.clone().unwrap_or_default(),
            "tags": tags,
            "projectUrl": n.project_url.clone().unwrap_or_default(),
            "licenseUrl": n.license_url.clone().unwrap_or_default(),
            "licenseExpression": n.license_expression.clone().unwrap_or_default(),
            "iconUrl": n.icon_url.clone().unwrap_or_default(),
            "requireLicenseAcceptance": n.require_license_acceptance,
            "dependencyGroups": groups,
            "listed": e.listed,
            "published": published(e),
            "packageContent": self.package_content(&e.id, &e.key),
        })
    }

    fn leaf_item(&self, e: &Entry) -> Value {
        json!({
            "@id": self.registration_leaf(&e.id, &e.key),
            "@type": "Package",
            "catalogEntry": self.catalog_entry(e),
            "packageContent": self.package_content(&e.id, &e.key),
            "registration": self.registration_index(&e.id),
        })
    }

    fn page(&self, id: &str, page: &[Entry], inline: bool) -> Value {
        let (lower, upper) = (&page[0].version, &page[page.len() - 1].version);
        let mut doc = json!({
            "@id": self.registration_page(id, lower, upper),
            "@type": "catalog:CatalogPage",
            "count": page.len(),
            "lower": lower,
            "upper": upper,
        });
        if inline {
            doc["items"] = page.iter().map(|e| self.leaf_item(e)).collect();
            doc["parent"] = json!(self.registration_index(id));
        }
        doc
    }

    /// Every version, listed or not; pages are inlined only up to the bound.
    pub fn registration_index_doc(&self, id: &str, entries: &[Entry]) -> Value {
        let inline = entries.len() <= INLINE_MAX;
        let pages: Vec<Value> = entries
            .chunks(PAGE_SIZE)
            .map(|p| self.page(id, p, inline))
            .collect();
        json!({
            "@id": self.registration_index(id),
            "@type": ["catalog:CatalogRoot", "PackageRegistration", "catalog:Permalink"],
            "count": pages.len(),
            "items": pages,
        })
    }

    /// The page whose bounds are `lower` and `upper`, with its leaves.
    pub fn registration_page_doc(&self, id: &str, entries: &[Entry], lower: &str, upper: &str) -> Option<Value> {
        let same = |a: &str, b: &str| {
            super::version::NuGetVersion::parse(a).ok().map(|v| v.key())
                == super::version::NuGetVersion::parse(b).ok().map(|v| v.key())
        };
        entries
            .chunks(PAGE_SIZE)
            .find(|p| same(&p[0].version, lower) && same(&p[p.len() - 1].version, upper))
            .map(|p| self.page(id, p, true))
    }

    pub fn registration_leaf_doc(&self, e: &Entry) -> Value {
        json!({
            "@id": self.registration_leaf(&e.id, &e.key),
            "@type": ["Package", "http://schema.nuget.org/catalog#Permalink"],
            "catalogEntry": e.catalog.clone().unwrap_or_else(|| format!("{}#catalog", self.registration_leaf(&e.id, &e.key))),
            "listed": e.listed,
            "packageContent": self.package_content(&e.id, &e.key),
            "published": published(e),
            "registration": self.registration_index(&e.id),
        })
    }

    /// One search hit: the package and its versions, newest last.
    pub fn search_hit(&self, entries: &[Entry], downloads: i64) -> Value {
        let latest = &entries[entries.len() - 1];
        let n = &latest.nuspec;
        let versions: Vec<Value> = entries
            .iter()
            .map(|e| json!({"@id": self.registration_leaf(&e.id, &e.key), "version": e.version, "downloads": 0}))
            .collect();
        let tags: Vec<&str> = n.tags.as_deref().unwrap_or("").split_whitespace().collect();
        let types: Vec<Value> = n.package_types.iter().map(|t| json!({"name": t.name})).collect();
        json!({
            "@id": self.registration_index(&latest.id),
            "@type": "Package",
            "registration": self.registration_index(&latest.id),
            "id": latest.id,
            "version": latest.version,
            "description": n.description.clone().unwrap_or_default(),
            "summary": n.summary.clone().unwrap_or_default(),
            "title": n.title.clone().unwrap_or_default(),
            "authors": n.authors.clone().map(|a| vec![a]).unwrap_or_default(),
            "tags": tags,
            "projectUrl": n.project_url.clone().unwrap_or_default(),
            "licenseUrl": n.license_url.clone().unwrap_or_default(),
            "iconUrl": n.icon_url.clone().unwrap_or_default(),
            "totalDownloads": downloads,
            "verified": false,
            "packageTypes": types,
            "versions": versions,
        })
    }
}

fn published(e: &Entry) -> String {
    match (e.listed, e.published) {
        (true, Some(p)) => p.to_rfc3339(),
        (true, None) => chrono::DateTime::UNIX_EPOCH.to_rfc3339(),
        (false, _) => UNLISTED_PUBLISHED.to_string(),
    }
}

/// The flat container's version list: every key, ascending.
pub fn flat_index(entries: &[Entry]) -> Value {
    json!({"versions": entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::nuget::nuspec::Nuspec;

    fn entry(key: &str, listed: bool) -> Entry {
        Entry {
            id: "My.Lib".into(),
            version: key.to_uppercase(),
            key: key.into(),
            listed,
            published: None,
            nuspec: Nuspec::default(),
            catalog: None,
        }
    }

    #[test]
    fn urls_hang_off_the_requested_repository_only() {
        let base = Base::new("https://reg.example/", "nu get", true);
        assert_eq!(
            base.package_content("My.Lib", "1.0.0-beta"),
            "https://reg.example/nu%20get/v3/flatcontainer/my.lib/1.0.0-beta/my.lib.1.0.0-beta.nupkg"
        );
        let index = base.service_index();
        let types: Vec<&str> = index["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["@type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"PackagePublish/2.0.0"));
        let proxy = Base::new("https://reg.example", "p", false).service_index();
        assert!(!proxy.to_string().contains("PackagePublish"));
    }

    #[test]
    fn every_version_is_listed_and_only_small_indexes_inline() {
        let base = Base::new("http://h", "r", true);
        let entries: Vec<Entry> = (0..130).map(|i| entry(&format!("1.0.{i}"), i % 2 == 0)).collect();
        let doc = base.registration_index_doc("My.Lib", &entries);
        assert_eq!(doc["count"], 3);
        let counts: u64 = doc["items"].as_array().unwrap().iter().map(|p| p["count"].as_u64().unwrap()).sum();
        assert_eq!(counts, 130, "every version, listed or not");
        assert!(doc["items"][0].get("items").is_none(), "past the bound, pages are not inlined");
        let page = base
            .registration_page_doc("My.Lib", &entries, "1.0.64", "1.0.127")
            .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 64);
        let small = base.registration_index_doc("My.Lib", &entries[..3]);
        let leaf = &small["items"][0]["items"][1];
        assert_eq!(leaf["catalogEntry"]["listed"], false);
        assert_eq!(leaf["catalogEntry"]["published"], UNLISTED_PUBLISHED);
        assert!(!small.to_string().contains("nuget.org"));
    }
}
