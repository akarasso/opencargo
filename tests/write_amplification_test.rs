//! What a publish costs the disk beyond the bytes it keeps.
//!
//! Every page a transaction dirties is appended to the write-ahead log whole,
//! so the frames one publish adds are its amplification in the database; the
//! log the process writes is the rest of it. Both are counted from the files,
//! which makes an index added to the publish path, or a line that renders for
//! nobody, visible here before it is visible in a benchmark.

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

/// The pin a placement takes and spends, the version row and its unique index
/// are what a publish must dirty; a b-tree added to either side shows up here
/// as frames.
///
/// Counted over twenty publishes into a package that already exists, so the
/// row is the steady-state one and not the one that also creates a package.
/// The ceiling is the measured cost (7.4) plus the margin a page split needs,
/// against 12.4 before 027 and 028.
#[tokio::test]
async fn a_publish_dirties_at_most_eight_database_pages() {
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
        dirtied <= 8 * PUBLISHES,
        "{dirtied} pages for {PUBLISHES} publishes, {} each",
        dirtied as f64 / PUBLISHES as f64
    );
}

/// The shipped log, as a server that is not a terminal writes it: the default
/// level says what happened, and nothing in the line is an escape sequence —
/// they were 38% of the benchmark's 2.5 MiB of log, rendered by nothing.
#[tokio::test]
async fn the_shipped_log_is_plain_text_at_the_default_level() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = tmp.path().join("opencargo.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nstorage_path = \"{}\"\n[database]\nurl = \"sqlite:{}?mode=rwc\"\n",
            tmp.path().join("storage").display(),
            tmp.path().join("opencargo.db").display()
        ),
    )
    .unwrap();

    let run = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencargo"))
        .args(["--config", config.to_str().unwrap(), "migrate"])
        .env_remove("RUST_LOG")
        .env_remove("OPENCARGO_CONFIG")
        .output()
        .await
        .expect("the built binary runs");
    assert!(run.status.success(), "migrate failed: {run:?}");

    let logged = String::from_utf8_lossy(&run.stdout).to_string()
        + &String::from_utf8_lossy(&run.stderr);
    assert!(
        logged.contains("Database migrations applied"),
        "the default level says what happened: {logged}"
    );
    assert!(
        !logged.contains('\u{1b}'),
        "a log nobody colours carries no escape: {logged:?}"
    );
}
