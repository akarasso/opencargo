//! What a publish costs the write-ahead log.
//!
//! Every page a transaction dirties is appended to the WAL whole, so the
//! frames one publish adds — not the bytes it keeps — are the write
//! amplification of a registry that publishes all day. The ceilings below are
//! counted from the file, which makes a table or an index added to the publish
//! path visible here before it is visible in a benchmark.

mod common;

use common::{build_cargo_publish_body, build_crate_data, hosted, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};
use serde_json::json;

/// The WAL header is 32 bytes and every frame is a 24-byte header and one
/// page; the page size is in the header, so nothing here assumes 4 KiB.
fn wal_frames(dir: &std::path::Path) -> u64 {
    let wal = dir.join("opencargo.db-wal");
    let Ok(meta) = std::fs::metadata(&wal) else {
        return 0;
    };
    let mut header = [0u8; 12];
    {
        use std::io::Read;
        let mut file = std::fs::File::open(&wal).expect("the write-ahead log is readable");
        if file.read_exact(&mut header).is_err() {
            return 0;
        }
    }
    let page = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as u64;
    assert!(page >= 512, "a write-ahead log declares its page size");
    meta.len().saturating_sub(32) / (page + 24)
}

async fn publish(base_url: &str, name: &str, version: &str) {
    let metadata = json!({
        "name": name,
        "vers": version,
        "deps": [],
        "features": {},
        "authors": [],
        "description": "write amplification fixture",
        "license": "MIT",
    })
    .to_string();
    let body = build_cargo_publish_body(&metadata, &build_crate_data());
    let response = reqwest::Client::new()
        .put(format!("{base_url}/crates-hosted/api/v1/crates/new"))
        .bearer_auth("test-token")
        .body(body)
        .send()
        .await
        .expect("the publish request failed");
    assert!(response.status().is_success(), "publish {version} refused");
}

/// The pin a placement takes and spends, the version row and its indexes are
/// what a publish must dirty; a b-tree added to either side shows up here as
/// frames.
///
/// Counted over twenty publishes into a package that already exists, so the
/// row is the steady-state one and not the one that also creates a package.
/// The ceiling is the measured cost (8.3) plus the margin a page split needs,
/// against 12.4 for the pin table's four b-trees before 027.
#[tokio::test]
async fn a_publish_dirties_at_most_nine_database_pages() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "crates-hosted",
            RepositoryFormat::Cargo,
            Visibility::Private,
        )],
        ..Default::default()
    })
    .await;
    let db_dir = server.tmp.path().to_path_buf();

    publish(&server.base_url, "amplification", "1.0.0").await;
    let before = wal_frames(&db_dir);
    assert!(before > 0, "the server writes through a write-ahead log");

    const PUBLISHES: u64 = 20;
    for i in 0..PUBLISHES {
        publish(&server.base_url, "amplification", &format!("1.1.{i}")).await;
    }
    let dirtied = wal_frames(&db_dir).saturating_sub(before);

    assert!(
        dirtied <= 9 * PUBLISHES,
        "{dirtied} pages for {PUBLISHES} publishes, {} each",
        dirtied as f64 / PUBLISHES as f64
    );
}
