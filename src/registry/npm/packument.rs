use std::collections::HashMap;

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::domain::Version;
use crate::error::AppResult;
use crate::registry::resolve::{CacheRepo, Outcome};
use crate::wire::wire_ts;

/// A packument before the handler rewrites its tarball URLs.
pub struct Packument {
    pub json: Value,
    pub stale: bool,
}

/// Build the packument of a hosted member from its own rows; tarball URLs
/// still carry the member name and are rewritten by the handler.
pub async fn hosted_packument(
    db: &SqlitePool,
    member: CacheRepo<'_>,
    package_name: &str,
    abbreviated: bool,
) -> AppResult<Outcome<Value>> {
    let Some(package) = crate::db::get_package(db, member.0.id, package_name).await? else {
        return Ok(Outcome::NotFound);
    };
    let versions = crate::db::get_versions(db, package.id).await?;
    let dist_tags_map = dist_tags_map(db, package.id, &versions).await?;

    let mut versions_map: HashMap<String, Value> = HashMap::new();
    let mut time_map: HashMap<String, String> = HashMap::new();
    time_map.insert("created".to_string(), wire_ts(package.created_at));
    time_map.insert("modified".to_string(), wire_ts(package.updated_at));
    for v in &versions {
        let mut meta: Value = serde_json::from_str(&v.metadata_json).unwrap_or(json!({}));
        if abbreviated {
            strip_to_abbreviated(&mut meta);
        }
        time_map.insert(v.version.clone(), wire_ts(v.published_at));
        versions_map.insert(v.version.clone(), meta);
    }

    Ok(Outcome::Found(json!({
        "_id": package_name,
        "name": package_name,
        "description": package.description,
        "dist-tags": dist_tags_map,
        "versions": versions_map,
        "time": time_map,
    })))
}

/// `tag -> version` from the `dist_tags` rows of a hosted package.
pub async fn dist_tags_map(
    db: &SqlitePool,
    package_id: i64,
    versions: &[Version],
) -> AppResult<HashMap<String, String>> {
    let dist_tags = crate::db::get_dist_tags(db, package_id).await?;
    Ok(dist_tags
        .iter()
        .filter_map(|dt| {
            let v = versions.iter().find(|v| v.id == dt.version_id)?;
            Some((dt.tag.clone(), v.version.clone()))
        })
        .collect())
}

/// Strip every version of a packument down to the install-v1 fields.
pub fn strip_versions_to_abbreviated(packument: &mut Value) {
    if let Some(versions) = packument.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (_key, version_meta) in versions.iter_mut() {
            strip_to_abbreviated(version_meta);
        }
    }
}

/// The fields of npmjs's abbreviated version object, no fewer: `os`,
/// `cpu`, `libc` and `deprecated` decide what npm installs.
const ABBREVIATED_FIELDS: [&str; 20] = [
    "name",
    "version",
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
    "acceptDependencies",
    "bin",
    "directories",
    "engines",
    "dist",
    "os",
    "cpu",
    "libc",
    "deprecated",
    "hasInstallScript",
    "funding",
];

fn strip_to_abbreviated(meta: &mut Value) {
    if let Some(obj) = meta.as_object_mut() {
        obj.retain(|key, _| ABBREVIATED_FIELDS.contains(&key.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviated_keeps_platform_gates_and_drops_prose() {
        let mut packument = json!({
            "versions": {
                "1.0.0": {
                    "name": "esbuild-linux-64",
                    "version": "1.0.0",
                    "os": ["linux"],
                    "cpu": ["x64"],
                    "libc": ["glibc"],
                    "deprecated": "use esbuild",
                    "hasInstallScript": true,
                    "optionalDependencies": {"a": "1"},
                    "peerDependenciesMeta": {"b": {"optional": true}},
                    "dist": {"tarball": "t"},
                    "description": "prose",
                    "readme": "prose",
                    "scripts": {"postinstall": "x"},
                    "_npmUser": {"name": "x"}
                }
            }
        });
        strip_versions_to_abbreviated(&mut packument);
        let v = &packument["versions"]["1.0.0"];
        for kept in [
            "os",
            "cpu",
            "libc",
            "deprecated",
            "hasInstallScript",
            "optionalDependencies",
            "peerDependenciesMeta",
            "dist",
        ] {
            assert!(v.get(kept).is_some(), "{kept} is part of install-v1");
        }
        for dropped in ["description", "readme", "scripts", "_npmUser"] {
            assert!(v.get(dropped).is_none(), "{dropped} is not");
        }
    }
}
