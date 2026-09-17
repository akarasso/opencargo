use std::collections::HashMap;

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::registry::resolve::{CacheRepo, Outcome};

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
    let dist_tags = crate::db::get_dist_tags(db, package.id).await?;

    let mut dist_tags_map: HashMap<String, String> = HashMap::new();
    for dt in &dist_tags {
        if let Some(v) = versions.iter().find(|v| v.id == dt.version_id) {
            dist_tags_map.insert(dt.tag.clone(), v.version.clone());
        }
    }

    let mut versions_map: HashMap<String, Value> = HashMap::new();
    let mut time_map: HashMap<String, String> = HashMap::new();
    time_map.insert("created".to_string(), package.created_at.clone());
    time_map.insert("modified".to_string(), package.updated_at.clone());
    for v in &versions {
        let mut meta: Value = serde_json::from_str(&v.metadata_json).unwrap_or(json!({}));
        if abbreviated {
            strip_to_abbreviated(&mut meta);
        }
        time_map.insert(v.version.clone(), v.published_at.clone());
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

/// Strip every version of a packument down to the install-v1 fields.
pub fn strip_versions_to_abbreviated(packument: &mut Value) {
    if let Some(versions) = packument.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (_key, version_meta) in versions.iter_mut() {
            strip_to_abbreviated(version_meta);
        }
    }
}

fn strip_to_abbreviated(meta: &mut Value) {
    const KEEP: [&str; 12] = [
        "name",
        "version",
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
        "bin",
        "directories",
        "engines",
        "dist",
        "bundleDependencies",
        "peerDependenciesMeta",
    ];
    if let Some(obj) = meta.as_object_mut() {
        obj.retain(|key, _| KEEP.contains(&key.as_str()));
    }
}
