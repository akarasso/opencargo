use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, Visibility,
};
use opencargo::server;

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
fn build_publish_body(
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

/// Start a test server on a random port.
///
/// Returns the base URL (e.g. `http://127.0.0.1:12345`), the join handle for
/// the server task, and the TempDir (kept alive so the directory is not deleted
/// while tests are running).
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("test.db");

    let db_url = format!(
        "sqlite:{}?mode=rwc",
        db_path.to_str().expect("non-utf8 temp path")
    );

    // We need a preliminary config to build state. The base_url will be
    // corrected once we know the actual port.
    let mut config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            base_url: "http://127.0.0.1:0".to_string(), // placeholder
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
        repositories: vec![RepositoryConfig {
            name: "test-npm".to_string(),
            repo_type: RepositoryType::Hosted,
            format: RepositoryFormat::Npm,
            visibility: Visibility::Public,
            upstream: None,
            members: None,
        }],
        ..Default::default()
    };

    // Bind a listener to get the actual port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind to random port");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{}", addr);

    // Now set the real base_url in config so tarball URLs are correct.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_check() {
    let (base_url, _handle, _tmp) = setup().await;

    let resp = reqwest::get(format!("{}/health/live", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body, json!({"status": "ok"}));
}

#[tokio::test]
async fn test_publish_and_get_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(resp.status(), StatusCode::OK, "publish failed: {:?}", resp.text().await);

    // Fetch metadata
    let resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .send()
        .await
        .expect("get metadata request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let meta: Value = resp.json().await.expect("invalid json");

    // Verify name
    assert_eq!(meta["name"], "@test/hello");

    // Verify dist-tags
    assert_eq!(meta["dist-tags"]["latest"], "1.0.0");

    // Verify version entry exists
    assert!(meta["versions"]["1.0.0"].is_object(), "version 1.0.0 not found in metadata");
    assert_eq!(meta["versions"]["1.0.0"]["version"], "1.0.0");

    // Verify tarball URL format
    let tarball_url = meta["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .expect("no tarball url");
    assert!(
        tarball_url.contains("/test-npm/@test/hello/-/hello-1.0.0.tgz"),
        "unexpected tarball URL: {}",
        tarball_url
    );
    assert!(
        tarball_url.starts_with(&base_url),
        "tarball URL should start with base_url"
    );
}

#[tokio::test]
async fn test_download_tarball() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Download the tarball
    let resp = client
        .get(format!(
            "{}/test-npm/@test/hello/-/hello-1.0.0.tgz",
            base_url
        ))
        .send()
        .await
        .expect("download request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let downloaded = resp.bytes().await.expect("failed to read tarball bytes");
    assert_eq!(
        downloaded.as_ref(),
        tarball.as_slice(),
        "downloaded tarball does not match the original"
    );
}

#[tokio::test]
async fn test_publish_duplicate_version() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // First publish — should succeed
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("first publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Second publish of the same version — should get 409 Conflict
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("second publish failed");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_search() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Search
    let resp = client
        .get(format!("{}/test-npm/-/v1/search?text=hello", base_url))
        .send()
        .await
        .expect("search request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let search_result: Value = resp.json().await.expect("invalid json");
    let objects = search_result["objects"]
        .as_array()
        .expect("objects should be an array");

    assert!(
        !objects.is_empty(),
        "search should return at least one result"
    );

    let found = objects
        .iter()
        .any(|o| o["package"]["name"].as_str() == Some("@test/hello"));
    assert!(found, "search results should contain @test/hello");

    assert_eq!(
        search_result["total"]
            .as_u64()
            .expect("total should be a number"),
        1
    );
}

#[tokio::test]
async fn test_abbreviated_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Full metadata request (no special Accept header)
    let full_resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .send()
        .await
        .expect("full metadata request failed");
    assert_eq!(full_resp.status(), StatusCode::OK);
    let full_meta: Value = full_resp.json().await.expect("invalid json");

    // Abbreviated metadata request
    let abbrev_resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .header("Accept", "application/vnd.npm.install-v1+json")
        .send()
        .await
        .expect("abbreviated metadata request failed");
    assert_eq!(abbrev_resp.status(), StatusCode::OK);

    // Verify content-type header
    let content_type = abbrev_resp
        .headers()
        .get("content-type")
        .expect("missing content-type header")
        .to_str()
        .expect("invalid content-type");
    assert!(
        content_type.contains("application/vnd.npm.install-v1+json"),
        "unexpected content-type: {}",
        content_type
    );

    let abbrev_meta: Value = abbrev_resp.json().await.expect("invalid json");

    // The abbreviated version should still have name, version, dist
    let abbrev_version = &abbrev_meta["versions"]["1.0.0"];
    assert!(abbrev_version.is_object(), "abbreviated version 1.0.0 missing");
    assert!(abbrev_version.get("name").is_some(), "abbreviated should have name");
    assert!(abbrev_version.get("version").is_some(), "abbreviated should have version");
    assert!(abbrev_version.get("dist").is_some(), "abbreviated should have dist");

    // The abbreviated version should NOT have "main" or "description"
    // (these are stripped by the server for install-optimized responses)
    assert!(
        abbrev_version.get("main").is_none(),
        "abbreviated should NOT have 'main' field"
    );
    assert!(
        abbrev_version.get("description").is_none(),
        "abbreviated should NOT have 'description' field"
    );

    // The full version should have those fields
    let full_version = &full_meta["versions"]["1.0.0"];
    assert!(
        full_version.get("main").is_some(),
        "full metadata should have 'main' field"
    );
    assert!(
        full_version.get("description").is_some(),
        "full metadata should have 'description' field"
    );
}

#[tokio::test]
async fn test_auth_required_for_publish() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Attempt to publish WITHOUT a Bearer token
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "publish without token should return 401"
    );
}
