use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, Visibility,
};
use opencargo::server;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a gzip'd tar archive in memory containing `package/package.json`.
fn build_tarball(package_json_content: &str) -> Vec<u8> {
    let mut archive_buf = Vec::new();
    {
        let encoder =
            flate2::write::GzEncoder::new(&mut archive_buf, flate2::Compression::default());
        let mut tar_builder = tar::Builder::new(encoder);

        let content_bytes = package_json_content.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("package/package.json").unwrap();
        header.set_size(content_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();

        tar_builder.append(&header, content_bytes).unwrap();
        tar_builder.into_inner().unwrap().finish().unwrap();
    }
    archive_buf
}

/// Build the JSON publish payload that mimics `npm publish`.
fn build_npm_publish_body(
    package_name: &str,
    version: &str,
    description: &str,
    tarball_data: &[u8],
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(tarball_data);
    let attachment_key = format!(
        "{}-{}.tgz",
        package_name.split('/').last().unwrap_or(package_name),
        version
    );

    json!({
        "name": package_name,
        "description": description,
        "dist-tags": { "latest": version },
        "versions": {
            version: {
                "name": package_name,
                "version": version,
                "description": description,
                "main": "index.js",
                "dist": {
                    "shasum": ""
                }
            }
        },
        "_attachments": {
            attachment_key: {
                "content_type": "application/octet-stream",
                "data": b64,
                "length": tarball_data.len()
            }
        }
    })
}

/// Build the binary body for Cargo publish requests.
///
/// Format:
///   4 bytes LE u32 -- JSON metadata length
///   N bytes        -- JSON metadata
///   4 bytes LE u32 -- crate file length
///   M bytes        -- .crate file (gzip'd tar)
fn build_cargo_publish_body(metadata_json: &str, crate_data: &[u8]) -> Vec<u8> {
    let json_bytes = metadata_json.as_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    body.extend_from_slice(json_bytes);
    body.extend_from_slice(&(crate_data.len() as u32).to_le_bytes());
    body.extend_from_slice(crate_data);
    body
}

/// Build a minimal .crate file (gzip compressed data).
fn build_crate_data() -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"fake crate content").unwrap();
    encoder.finish().unwrap()
}

/// Publish an npm package to the test-npm repository.
async fn publish_npm_package(
    client: &reqwest::Client,
    base_url: &str,
    name: &str,
    version: &str,
    description: &str,
) {
    let pkg_json = format!(
        r#"{{"name":"{}","version":"{}","description":"{}","main":"index.js"}}"#,
        name, version, description
    );
    let tarball = build_tarball(&pkg_json);
    let body = build_npm_publish_body(name, version, description, &tarball);

    let resp = client
        .put(format!("{}/test-npm/{}", base_url, name))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "npm publish failed: {:?}",
        resp.text().await
    );
}

/// Start a test server on a random port with both npm and cargo repositories.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("test.db");

    let db_url = format!(
        "sqlite:{}?mode=rwc",
        db_path.to_str().expect("non-utf8 temp path")
    );

    let mut config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            base_url: "http://127.0.0.1:0".to_string(),
            storage_path: storage_path
                .to_str()
                .expect("non-utf8 temp path")
                .to_string(),
            ..Default::default()
        },
        database: DatabaseConfig { url: db_url },
        auth: AuthConfig {
            anonymous_read: true,
            static_tokens: vec!["test-token".to_string()],
            ..Default::default()
        },
        repositories: vec![
            RepositoryConfig {
                name: "test-npm".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "cargo-private".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Cargo,
                visibility: Visibility::Private,
                upstream: None,
                members: None,
            },
        ],
        ..Default::default()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind to random port");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{}", addr);

    config.server.base_url = base_url.clone();

    let state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let router = server::build_router(state);

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Wait for the server to be ready by polling /health/live.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{}/health/live", &base_url)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

// ===========================================================================
// Phase 4 -- UI Tests (SPA + JSON API)
// ===========================================================================

#[tokio::test]
async fn test_dashboard_page() {
    let (base_url, _handle, _tmp) = setup().await;

    // The SPA shell should be served at /
    let resp = reqwest::get(format!("{}/", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("opencargo"),
        "dashboard should contain 'opencargo'"
    );
    // SPA serves index.html with a script tag that loads the app
    assert!(
        body.contains("<div id=\"app\">"),
        "SPA shell should contain app mount point"
    );
}

#[tokio::test]
async fn test_dashboard_api() {
    let (base_url, _handle, _tmp) = setup().await;

    let resp = reqwest::get(format!("{}/api/v1/dashboard", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    assert!(
        data.get("total_packages").is_some(),
        "dashboard API should return total_packages"
    );
    assert!(
        data.get("total_repos").is_some(),
        "dashboard API should return total_repos"
    );
    assert!(
        data.get("recent_versions").is_some(),
        "dashboard API should return recent_versions"
    );
}

#[tokio::test]
async fn test_packages_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish a package first
    publish_npm_package(
        &client,
        &base_url,
        "@test/pkglist",
        "1.0.0",
        "A package for listing test",
    )
    .await;

    // SPA shell is served at /packages
    let resp = client
        .get(format!("{}/packages", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/packages", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    let packages = data["packages"].as_array().expect("packages should be an array");
    assert!(
        packages.iter().any(|p| p["name"].as_str() == Some("@test/pkglist")),
        "packages API should contain the published package name, data: {:?}",
        data
    );
}

#[tokio::test]
async fn test_package_detail_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish @test/uipkg
    publish_npm_package(
        &client,
        &base_url,
        "@test/uipkg",
        "2.0.0",
        "UI package test",
    )
    .await;

    // SPA shell is served at /packages/*
    let resp = client
        .get(format!("{}/packages/@test/uipkg", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/packages/@test/uipkg", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    assert_eq!(
        data["name"].as_str(),
        Some("@test/uipkg"),
        "package detail API should return '@test/uipkg'"
    );
    let versions = data["versions"].as_array().expect("versions should be an array");
    assert!(
        versions.iter().any(|v| v["version"].as_str() == Some("2.0.0")),
        "package detail should contain version '2.0.0'"
    );
}

#[tokio::test]
async fn test_search_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish @test/searchable
    publish_npm_package(
        &client,
        &base_url,
        "@test/searchable",
        "1.0.0",
        "A searchable package",
    )
    .await;

    // SPA shell is served at /search
    let resp = client
        .get(format!("{}/search?q=searchable", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/search?q=searchable", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    let results = data["results"].as_array().expect("results should be an array");
    assert!(
        results.iter().any(|r| r["name"].as_str() == Some("@test/searchable")),
        "search API should contain '@test/searchable'"
    );
}

#[tokio::test]
async fn test_static_css() {
    let (base_url, _handle, _tmp) = setup().await;

    // First, get the SPA shell to find the CSS asset path
    let resp = reqwest::get(format!("{}/", base_url))
        .await
        .expect("request failed");

    let body = resp.text().await.expect("failed to read body");

    // Extract the CSS asset path from the HTML (e.g., /assets/index-XXXX.css)
    let css_path = body
        .split("href=\"")
        .find(|s| s.contains(".css"))
        .and_then(|s| s.split('"').next())
        .expect("should find a CSS asset link in the SPA shell");

    let resp = reqwest::get(format!("{}{}", base_url, css_path))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type header")
        .to_str()
        .expect("invalid content-type");

    assert!(
        content_type.contains("css"),
        "content-type should contain 'css', got: {}",
        content_type
    );
}

// ===========================================================================
// Phase 5 -- Metrics Tests
// ===========================================================================

#[tokio::test]
async fn test_prometheus_metrics() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Make some requests to generate metrics
    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("health request failed");

    // GET /metrics
    let resp = client
        .get(format!("{}/metrics", base_url))
        .send()
        .await
        .expect("metrics request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read metrics body");
    assert!(
        body.contains("opencargo_http_requests_total"),
        "metrics should contain 'opencargo_http_requests_total', body: {}",
        body
    );
    assert!(
        body.contains("opencargo_http_request_duration_seconds"),
        "metrics should contain 'opencargo_http_request_duration_seconds', body: {}",
        body
    );
}

#[tokio::test]
async fn test_metrics_counts_requests() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Make at least 2 requests to /health/live
    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("first health request failed");

    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("second health request failed");

    // GET /metrics
    let resp = client
        .get(format!("{}/metrics", base_url))
        .send()
        .await
        .expect("metrics request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read metrics body");

    // Find the counter line for path="/health/live"
    // The Prometheus text format looks like:
    //   opencargo_http_requests_total{method="GET",path="/health/live",status="200"} 2
    let found = body.lines().any(|line| {
        line.contains("opencargo_http_requests_total")
            && line.contains("path=\"/health/live\"")
            && !line.starts_with('#')
            && {
                // Parse the numeric value at the end of the line
                if let Some(val_str) = line.split_whitespace().last() {
                    if let Ok(val) = val_str.parse::<f64>() {
                        val >= 2.0
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
    });

    assert!(
        found,
        "metrics should show at least 2 requests to /health/live, metrics:\n{}",
        body
    );
}

// ===========================================================================
// Phase 6 -- Cargo Registry Tests
// ===========================================================================

#[tokio::test]
async fn test_cargo_config_json() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/cargo-private/index/config.json", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("config.json request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let config: Value = resp.json().await.expect("invalid JSON");
    assert!(
        config.get("dl").is_some(),
        "config.json should have 'dl' field: {:?}",
        config
    );
    assert!(
        config.get("api").is_some(),
        "config.json should have 'api' field: {:?}",
        config
    );

    // Verify the dl URL contains the repo name
    let dl = config["dl"].as_str().expect("dl should be a string");
    assert!(
        dl.contains("cargo-private"),
        "dl URL should contain 'cargo-private': {}",
        dl
    );
}

#[tokio::test]
async fn test_cargo_publish_and_download() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!(
            "{}/cargo-private/api/v1/crates/new",
            base_url
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Download
    let resp = client
        .get(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/download",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("cargo download request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let downloaded = resp.bytes().await.expect("failed to read crate bytes");
    assert_eq!(
        downloaded.as_ref(),
        crate_data.as_slice(),
        "downloaded crate data should match the original"
    );
}

#[tokio::test]
async fn test_cargo_index_entry() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!(
            "{}/cargo-private/api/v1/crates/new",
            base_url
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Fetch index entry: "test-crate" is 10 chars, prefix = te/st
    let resp = client
        .get(format!(
            "{}/cargo-private/index/te/st/test-crate",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body_text = resp.text().await.expect("failed to read index body");

    // The index entry should contain a JSON line with name and vers
    assert!(
        body_text.contains(r#""name":"test-crate""#),
        "index entry should contain '\"name\":\"test-crate\"', got: {}",
        body_text
    );
    assert!(
        body_text.contains(r#""vers":"0.1.0""#),
        "index entry should contain '\"vers\":\"0.1.0\"', got: {}",
        body_text
    );
}

#[tokio::test]
async fn test_cargo_yank_unyank() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!(
            "{}/cargo-private/api/v1/crates/new",
            base_url
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Yank
    let resp = client
        .delete(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/yank",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("yank request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify index shows yanked:true
    let resp = client
        .get(format!(
            "{}/cargo-private/index/te/st/test-crate",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request after yank failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body_text = resp.text().await.expect("failed to read index body");
    assert!(
        body_text.contains(r#""yanked":true"#),
        "index entry should contain '\"yanked\":true' after yank, got: {}",
        body_text
    );

    // Unyank
    let resp = client
        .put(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/unyank",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("unyank request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify index shows yanked:false
    let resp = client
        .get(format!(
            "{}/cargo-private/index/te/st/test-crate",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request after unyank failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body_text = resp.text().await.expect("failed to read index body");
    assert!(
        body_text.contains(r#""yanked":false"#),
        "index entry should contain '\"yanked\":false' after unyank, got: {}",
        body_text
    );
}
