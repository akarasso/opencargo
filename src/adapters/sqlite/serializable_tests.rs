//! The port's serializable inventory, checked against this adapter's
//! sources: a function that writes a table `ReferencedKeys` reads and is
//! not mapped onto a listed port method fails here.
//!
//! A statement whose table is interpolated counts as a write whatever it
//! names, so no dynamic spelling escapes the inventory.

use std::collections::BTreeSet;

use crate::ports::reclaim::SERIALIZABLE;

/// `(file, function, the port method it serves)`.
const WRITERS: &[(&str, &str, &str)] = &[
    ("maven.rs", "put_file", "MavenFileStore::change"),
    ("maven.rs", "refuse_unit", "MavenFileStore::refuse"),
    ("oci.rs", "claim_one_segment", "OciStore::claim_segment"),
    ("oci.rs", "drop_blob", "OciStore::delete_blob"),
    ("oci.rs", "finish", "OciStore::finish_upload"),
    ("oci.rs", "lease", "OciStore::begin_complete"),
    ("oci.rs", "purge_manifest", "OciStore::delete_manifest"),
    ("oci.rs", "reap", "OciStore::reap_uploads"),
    ("oci.rs", "release_complete", "OciStore::release_complete"),
    ("oci.rs", "start_upload", "OciStore::start_upload"),
    ("oci.rs", "write_manifest", "OciStore::put_manifest"),
    ("packages.rs", "purge_version", "PackageStore::delete_version"),
    ("packages.rs", "set_metadata", "PackageStore::set_metadata"),
    ("packages.rs", "set_yanked", "PackageStore::set_yanked"),
    ("packages.rs", "write_release", "PackageStore::publish_version"),
    ("packages.rs", "write_release", "PackageStore::promote_metadata"),
    ("proxy_cache.rs", "delete", "ProxyCacheStore::delete"),
    ("proxy_cache.rs", "delete_for_repo", "ProxyCacheStore::delete_for_repo"),
    ("proxy_cache.rs", "quarantined", "ProxyCacheStore::quarantined"),
    ("proxy_cache.rs", "touch", "ProxyCacheStore::touch"),
    ("proxy_cache.rs", "upsert", "ProxyCacheStore::upsert"),
    ("pypi.rs", "purge", "PypiFileStore::delete_release"),
    ("pypi.rs", "purge", "PypiFileStore::delete_project_files"),
    ("pypi.rs", "set_release_yanked", "PypiFileStore::set_release_yanked"),
    ("pypi.rs", "version_id", "PypiFileStore::publish_file"),
    ("pypi.rs", "write_file", "PypiFileStore::publish_file"),
];

/// Interpolated statements that run under the migration lock, with no
/// concurrent writer and no claim to race: a schema rebuild and an identity
/// migration, neither of them a port method.
const MIGRATIONS: &[(&str, &str)] = &[
    ("identities.rs", "migrate_authority_in"),
    ("rebuild.rs", "rebuild"),
];

/// The tables `REFERENCED` reads, as table names.
const TABLES: &[&str] = &[
    "versions",
    "proxy_cache_entries",
    "oci_blobs",
    "oci_manifests",
    "oci_uploads",
    "maven_files",
    "pypi_files",
];

fn writes(line: &str) -> bool {
    let upper = line.to_uppercase();
    // A statement whose table is interpolated counts whatever it names.
    if ["INTO {", "UPDATE {", "DELETE FROM {"]
        .iter()
        .any(|dynamic| upper.contains(dynamic))
    {
        return true;
    }
    TABLES.iter().any(|table| {
        let table = table.to_uppercase();
        [
            format!("INTO {table}"),
            format!("UPDATE {table}"),
            format!("DELETE FROM {table}"),
        ]
        .iter()
        .any(|statement| upper.contains(statement.as_str()))
    })
}

fn function_of(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Every function of this adapter that writes one of those tables.
fn scanned() -> BTreeSet<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/sqlite");
    let mut found = BTreeSet::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("the adapter's sources are readable")
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|name| name.ends_with(".rs") && !name.ends_with("_tests.rs"))
        .collect();
    files.sort();
    for file in files {
        let source = std::fs::read_to_string(dir.join(&file)).expect("readable");
        let mut current = String::new();
        for line in source.lines() {
            if let Some(name) = function_of(line) {
                current = name;
            }
            if writes(line)
                && !current.is_empty()
                && !MIGRATIONS.contains(&(file.as_str(), current.as_str()))
            {
                found.insert((file.clone(), current.clone()));
            }
        }
    }
    found
}

#[test]
fn every_writer_of_a_referenced_table_is_in_the_serializable_inventory() {
    let declared: BTreeSet<(String, String)> = WRITERS
        .iter()
        .map(|(file, function, _)| ((*file).to_string(), (*function).to_string()))
        .collect();
    assert_eq!(
        scanned(),
        declared,
        "a method that writes a table ReferencedKeys reads must be mapped onto \
         the port method it serves, and that method must be in the inventory"
    );
    for (_, _, method) in WRITERS {
        assert!(
            SERIALIZABLE.contains(method),
            "{method} writes a referenced table and is not in the inventory"
        );
    }
}

/// The inventory is the port's, so nothing in it is unreachable here.
#[test]
fn the_inventory_names_no_method_this_adapter_does_not_have() {
    for method in SERIALIZABLE {
        let (port, name) = method.split_once("::").expect("Port::method");
        let known = WRITERS.iter().any(|(_, _, m)| m == method)
            || matches!(port, "ReclaimStore")
            || name.is_empty();
        assert!(known, "{method} is in the inventory and writes nothing here");
    }
}
