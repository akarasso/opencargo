//! The raw format driven by a real `curl`: upload, download, checksum,
//! listing and delete, exactly as the quickstart prints them.
//!
//! Skipped when `curl` is absent, unless `OPENCARGO_E2E_REQUIRE=1`.

mod common;

use std::ffi::OsStr;
use std::path::Path;

use tempfile::TempDir;

use common::{client_bin, group, hosted, run_cmd, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const PATH: &str = "dist/linux-amd64/tool-1.2.3.tar.gz";

/// curl with the credentials every step sends, failing on a 4xx/5xx
/// (`--fail-with-body`) so a refused step is a refused command.
async fn curl(bin: &str, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let mut all = vec!["--silent", "--show-error", "--fail-with-body", "--user"];
    let user = format!("admin:{STATIC_TOKEN}");
    all.push(&user);
    all.extend_from_slice(args);
    let env: [(&str, &OsStr); 0] = [];
    run_cmd(bin, &all, cwd, &env).await
}

async fn spawn() -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("files", RepositoryFormat::Raw, Visibility::Private),
            group("all-files", RepositoryFormat::Raw, &["files"]),
        ],
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn curl_uploads_downloads_lists_and_deletes_a_file() {
    let Some(curl_bin) = client_bin("CURL_BIN") else {
        return;
    };
    let s = spawn().await;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let payload = vec![b'x'; 128 * 1024];
    std::fs::write(dir.join("tool.tar.gz"), &payload).unwrap();
    let sha = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&payload));
    let url = format!("{}/raw/files/{PATH}", s.base_url);

    let (ok, _, err) = curl(&curl_bin, dir, &["-T", "tool.tar.gz", &url]).await;
    assert!(ok, "upload: {err}");

    let (ok, _, err) = curl(&curl_bin, dir, &["-o", "back.tar.gz", &url]).await;
    assert!(ok, "download: {err}");
    assert_eq!(std::fs::read(dir.join("back.tar.gz")).unwrap(), payload);

    let (ok, headers, err) = curl(&curl_bin, dir, &["-I", &url]).await;
    assert!(ok, "head: {err}");
    let headers = headers.to_ascii_lowercase();
    assert!(headers.contains(&format!("x-checksum-sha256: {sha}")), "{headers}");
    assert!(headers.contains("content-length: 131072"), "{headers}");

    let listing = format!("{}/api/v1/raw/all-files/files?prefix=dist", s.base_url);
    let (ok, body, err) = curl(&curl_bin, dir, &[&listing]).await;
    assert!(ok, "listing: {err}");
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["total"], 1, "{body}");
    assert_eq!(listed["files"][0]["path"], PATH);
    assert_eq!(listed["files"][0]["repository"], "files", "the group names its member");
    assert_eq!(listed["files"][0]["sha256"], sha);
    assert_eq!(listed["files"][0]["size"], 131_072);

    let (ok, body, err) = curl(&curl_bin, dir, &["-X", "DELETE", &url]).await;
    assert!(ok, "delete: {err} {body}");
    let (gone, _, _) = curl(&curl_bin, dir, &["-o", "gone", &url]).await;
    assert!(!gone, "a deleted path is a 404");
}

#[tokio::test]
async fn curl_refuses_a_body_that_does_not_match_the_checksum_it_declares() {
    let Some(curl_bin) = client_bin("CURL_BIN") else {
        return;
    };
    let s = spawn().await;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("tool.tar.gz"), b"payload").unwrap();
    let url = format!("{}/raw/files/{PATH}", s.base_url);
    let wrong = format!("x-checksum-sha256: {}", "a".repeat(64));

    let (ok, _, _) = curl(&curl_bin, dir, &["-T", "tool.tar.gz", "-H", &wrong, &url]).await;
    assert!(!ok, "a wrong declared checksum is refused");

    let right = format!(
        "x-checksum-sha256: {:x}",
        <sha2::Sha256 as sha2::Digest>::digest(b"payload")
    );
    let (ok, _, err) = curl(&curl_bin, dir, &["-T", "tool.tar.gz", "-H", &right, &url]).await;
    assert!(ok, "the right one is accepted: {err}");
}
