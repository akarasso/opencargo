use std::collections::BTreeMap;

use serde_json::value::RawValue;
use serde_json::Value;

/// Which packument a client asked for: npm's install-v1 subset, or the
/// whole document.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    Abbreviated,
    Full,
}

pub const ABBREVIATED_TYPE: &str = "application/vnd.npm.install-v1+json";

impl Flavor {
    pub fn of(abbreviated: bool) -> Self {
        if abbreviated {
            Flavor::Abbreviated
        } else {
            Flavor::Full
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Flavor::Abbreviated => ABBREVIATED_TYPE,
            Flavor::Full => "application/json",
        }
    }

    /// One path-safe token, for the key a rendering is remembered under.
    pub fn tag(self) -> &'static str {
        match self {
            Flavor::Abbreviated => "install-v1",
            Flavor::Full => "full",
        }
    }
}

/// Where this server serves a package's tarball from.
pub fn tarball_url(base_url: &str, repo: &str, package: &str, filename: &str) -> String {
    format!(
        "{}/{repo}/{package}/-/{filename}",
        base_url.trim_end_matches('/')
    )
}

/// Point every `dist.tarball` of a packument tree at this server.
pub fn rewrite_tarball_urls(metadata: &mut Value, base_url: &str, repo: &str, package: &str) {
    if let Some(versions) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (_version, meta) in versions.iter_mut() {
            rewrite_one(meta, base_url, repo, package);
        }
    }
}

fn rewrite_one(meta: &mut Value, base_url: &str, repo: &str, package: &str) {
    let Some(dist) = meta.get_mut("dist").and_then(|d| d.as_object_mut()) else {
        return;
    };
    let Some(filename) = dist
        .get("tarball")
        .and_then(|t| t.as_str())
        .and_then(|t| t.rsplit('/').next())
        .map(str::to_string)
    else {
        return;
    };
    dist.insert(
        "tarball".to_string(),
        Value::String(tarball_url(base_url, repo, package, &filename)),
    );
}

/// The document this server serves for an upstream packument: tarball URLs
/// pointing here and, for install-v1, versions stripped to its fields.
///
/// Only one version object is a tree at a time. A packument is mostly its
/// `versions` map, and a `Value` over the whole of it costs several times
/// the bytes it was parsed from -- which a proxy would hold once per
/// concurrent reader.
pub fn render(
    raw: &[u8],
    flavor: Flavor,
    base_url: &str,
    repo: &str,
    package: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let doc: BTreeMap<String, &RawValue> = serde_json::from_slice(raw)?;
    let mut out = Vec::with_capacity(raw.len());
    out.push(b'{');
    for (i, (key, value)) in doc.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        match key.as_str() {
            "versions" => write_versions(&mut out, value, flavor, base_url, repo, package)?,
            _ => out.extend_from_slice(value.get().as_bytes()),
        }
    }
    out.push(b'}');
    Ok(out)
}

/// One top-level field of a packument, parsed on its own: the rest of the
/// document stays the bytes it arrived as.
pub fn field(raw: &[u8], name: &str) -> Result<Option<Value>, serde_json::Error> {
    let doc: BTreeMap<String, &RawValue> = serde_json::from_slice(raw)?;
    doc.get(name)
        .map(|value| serde_json::from_str(value.get()))
        .transpose()
}

/// A `versions` that is not an object is copied as it came: the tree path
/// left such a document alone too.
fn write_versions(
    out: &mut Vec<u8>,
    versions: &RawValue,
    flavor: Flavor,
    base_url: &str,
    repo: &str,
    package: &str,
) -> Result<(), serde_json::Error> {
    let Ok(map) = serde_json::from_str::<BTreeMap<String, &RawValue>>(versions.get()) else {
        out.extend_from_slice(versions.get().as_bytes());
        return Ok(());
    };
    out.push(b'{');
    for (i, (version, raw)) in map.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut *out, version)?;
        out.push(b':');
        let mut meta: Value = serde_json::from_str(raw.get())?;
        if flavor == Flavor::Abbreviated {
            super::packument::strip_to_abbreviated(&mut meta);
        }
        rewrite_one(&mut meta, base_url, repo, package);
        serde_json::to_writer(&mut *out, &meta)?;
    }
    out.push(b'}');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source() -> Value {
        json!({
            "_id": "widget",
            "name": "widget",
            "dist-tags": {"latest": "2.0.0"},
            "time": {"created": "2026-01-01T00:00:00Z"},
            "readme": "prose",
            "versions": {
                "2.0.0": {
                    "name": "widget",
                    "version": "2.0.0",
                    "description": "prose",
                    "scripts": {"test": "x"},
                    "dependencies": {"left-pad": "^1"},
                    "dist": {
                        "tarball": "https://registry.npmjs.org/widget/-/widget-2.0.0.tgz",
                        "integrity": "sha512-deadbeef"
                    }
                },
                "1.0.0": {
                    "name": "widget",
                    "version": "1.0.0",
                    "dist": {"tarball": "https://registry.npmjs.org/widget/-/widget-1.0.0.tgz"}
                }
            }
        })
    }

    fn rendered(flavor: Flavor) -> Value {
        let raw = serde_json::to_vec(&source()).unwrap();
        let out = render(&raw, flavor, "http://here/", "npm-proxy", "widget").unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    /// The tree path this replaces: parse the whole document, strip it,
    /// rewrite it, serialise it.
    fn through_a_tree(flavor: Flavor) -> Value {
        let mut doc = source();
        if flavor == Flavor::Abbreviated {
            super::super::packument::strip_versions_to_abbreviated(&mut doc);
        }
        rewrite_tarball_urls(&mut doc, "http://here/", "npm-proxy", "widget");
        doc
    }

    #[test]
    fn a_rendering_is_what_the_tree_path_produced() {
        for flavor in [Flavor::Abbreviated, Flavor::Full] {
            assert_eq!(rendered(flavor), through_a_tree(flavor), "{flavor:?}");
        }
    }

    #[test]
    fn tarballs_point_here_and_install_v1_drops_prose() {
        let doc = rendered(Flavor::Abbreviated);
        assert_eq!(
            doc["versions"]["2.0.0"]["dist"]["tarball"],
            json!("http://here/npm-proxy/widget/-/widget-2.0.0.tgz")
        );
        assert_eq!(
            doc["versions"]["2.0.0"]["dist"]["integrity"],
            json!("sha512-deadbeef")
        );
        assert!(doc["versions"]["2.0.0"].get("description").is_none());
        assert!(doc["versions"]["2.0.0"].get("scripts").is_none());
        assert_eq!(doc["readme"], json!("prose"), "the document keeps its own fields");
        assert_eq!(doc["dist-tags"]["latest"], json!("2.0.0"));
    }

    #[test]
    fn the_full_flavor_keeps_every_version_field() {
        let doc = rendered(Flavor::Full);
        assert_eq!(doc["versions"]["2.0.0"]["description"], json!("prose"));
        assert_eq!(
            doc["versions"]["1.0.0"]["dist"]["tarball"],
            json!("http://here/npm-proxy/widget/-/widget-1.0.0.tgz")
        );
    }

    #[test]
    fn a_versions_that_is_not_a_map_is_left_as_it_came() {
        let raw = serde_json::to_vec(&json!({"name": "widget", "versions": null})).unwrap();
        let out = render(&raw, Flavor::Abbreviated, "http://here", "r", "widget").unwrap();
        let doc: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["versions"], Value::Null);
    }

    #[test]
    fn a_body_that_is_not_a_packument_is_refused() {
        assert!(render(b"[1,2,3]", Flavor::Full, "http://here", "r", "widget").is_err());
        assert!(render(b"not json", Flavor::Full, "http://here", "r", "widget").is_err());
    }

    #[test]
    fn a_version_without_a_tarball_is_untouched() {
        let raw = serde_json::to_vec(&json!({
            "name": "widget",
            "versions": {"1.0.0": {"version": "1.0.0", "dist": {"integrity": "sha512-x"}}}
        }))
        .unwrap();
        let out = render(&raw, Flavor::Full, "http://here", "r", "widget").unwrap();
        let doc: Value = serde_json::from_slice(&out).unwrap();
        assert!(doc["versions"]["1.0.0"]["dist"].get("tarball").is_none());
    }
}
