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
// Helpers (reused from npm_test.rs)
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

/// A well-known small **scoped** package on npmjs.org.
///
/// The server only exposes GET metadata for scoped packages
/// (`/{repo}/@{scope}/{name}`), so we use a real scoped package for proxy
/// tests.  `@pnpm/error` is tiny and has very few versions.
const PROXY_TEST_PACKAGE: &str = "@pnpm/error";
const PROXY_TEST_SCOPE: &str = "pnpm";
const PROXY_TEST_NAME: &str = "error";

/// Start a test server configured with hosted, proxy, and group repositories.
///
/// Returns `(base_url, join_handle, temp_dir)`.
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
        repositories: vec![
            RepositoryConfig {
                name: "npm-private".into(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Private,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "npm-proxy".into(),
                repo_type: RepositoryType::Proxy,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: Some("https://registry.npmjs.org".into()),
                members: None,
            },
            RepositoryConfig {
                name: "npm-group".into(),
                repo_type: RepositoryType::Group,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: Some(vec!["npm-private".into(), "npm-proxy".into()]),
            },
        ],
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

/// Fetch metadata for a real scoped npm package through the proxy repository.
///
/// Asserts that the response contains valid metadata (versions, dist-tags)
/// and that the tarball URLs have been rewritten to point to our server.
#[tokio::test]
async fn test_proxy_fetches_from_upstream() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{}/npm-proxy/@{}/{}",
            base_url, PROXY_TEST_SCOPE, PROXY_TEST_NAME
        ))
        .send()
        .await
        .expect("proxy metadata request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "proxy should return 200 for a real npm package"
    );

    let meta: Value = resp.json().await.expect("invalid json from proxy");

    // Should have versions
    let versions = meta
        .get("versions")
        .and_then(|v| v.as_object())
        .expect("metadata should contain a 'versions' object");
    assert!(
        !versions.is_empty(),
        "versions should not be empty for {}",
        PROXY_TEST_PACKAGE
    );

    // Should have dist-tags
    let dist_tags = meta.get("dist-tags").and_then(|v| v.as_object());
    assert!(
        dist_tags.is_some(),
        "metadata should contain 'dist-tags'"
    );

    // Pick the first version and check that the tarball URL points to OUR server
    let (_, first_version_meta) = versions.iter().next().unwrap();
    let tarball_url = first_version_meta
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(|t| t.as_str())
        .expect("version should have dist.tarball");

    assert!(
        tarball_url.starts_with(&base_url),
        "tarball URL should point to our server, got: {}",
        tarball_url
    );
    assert!(
        !tarball_url.contains("registry.npmjs.org"),
        "tarball URL should NOT contain the upstream host, got: {}",
        tarball_url
    );
    assert!(
        tarball_url.contains("/npm-proxy/"),
        "tarball URL should contain the repo name, got: {}",
        tarball_url
    );
}

/// Fetch the same package twice; the second request should be served from
/// cache and both responses should be identical.
#[tokio::test]
async fn test_proxy_caches_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let url = format!(
        "{}/npm-proxy/@{}/{}",
        base_url, PROXY_TEST_SCOPE, PROXY_TEST_NAME
    );

    // First fetch (populates cache)
    let start1 = std::time::Instant::now();
    let resp1 = client
        .get(&url)
        .send()
        .await
        .expect("first proxy request failed");
    let elapsed1 = start1.elapsed();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1: Value = resp1.json().await.expect("invalid json (first)");

    // Second fetch (should come from cache)
    let start2 = std::time::Instant::now();
    let resp2 = client
        .get(&url)
        .send()
        .await
        .expect("second proxy request failed");
    let elapsed2 = start2.elapsed();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value = resp2.json().await.expect("invalid json (second)");

    // Both responses should be identical
    assert_eq!(
        body1, body2,
        "cached response should be identical to the original"
    );

    // The cached request should generally be faster, but we just log this
    // rather than assert it — timing can be flaky in CI.
    eprintln!(
        "First request: {:?}, Second (cached) request: {:?}",
        elapsed1, elapsed2
    );
}

/// Fetch metadata from the proxy and then download one of the tarballs.
/// Verifies that the downloaded data is non-empty and starts with the
/// gzip magic bytes (0x1f 0x8b).
#[tokio::test]
async fn test_proxy_download_tarball() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Step 1: fetch metadata to discover a valid tarball URL
    let resp = client
        .get(format!(
            "{}/npm-proxy/@{}/{}",
            base_url, PROXY_TEST_SCOPE, PROXY_TEST_NAME
        ))
        .send()
        .await
        .expect("proxy metadata request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let meta: Value = resp.json().await.expect("invalid json");
    let versions = meta["versions"]
        .as_object()
        .expect("versions should be an object");

    // Pick the first available version's tarball URL
    let (_, version_meta) = versions.iter().next().expect("no versions found");
    let tarball_url = version_meta["dist"]["tarball"]
        .as_str()
        .expect("no tarball URL in dist");

    // Sanity: the URL should point to our server
    assert!(
        tarball_url.starts_with(&base_url),
        "tarball URL should be rewritten to our server"
    );

    // Step 2: download the tarball
    let resp = client
        .get(tarball_url)
        .send()
        .await
        .expect("tarball download request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tarball download should succeed"
    );

    let data = resp.bytes().await.expect("failed to read tarball bytes");
    assert!(
        !data.is_empty(),
        "tarball data should not be empty"
    );

    // Gzip magic bytes
    assert_eq!(
        data[0], 0x1f,
        "tarball should start with gzip magic byte 0x1f"
    );
    assert_eq!(
        data[1], 0x8b,
        "tarball second byte should be gzip magic 0x8b"
    );
}

/// With a group repo [npm-private, npm-proxy]:
/// - Publish `@test/mylib` to npm-private (the hosted member)
/// - GET `@test/mylib` from npm-group
///
/// Because npm-private is listed first in the group, the hosted version
/// should be returned.
#[tokio::test]
async fn test_group_repo_serves_hosted_first() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish @test/mylib to the hosted repo
    let pkg_json =
        r#"{"name":"@test/mylib","version":"1.0.0","description":"My private lib","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/mylib", "1.0.0", "My private lib", &tarball);

    let resp = client
        .put(format!("{}/npm-private/@test/mylib", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish to npm-private failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "publish should succeed: {:?}",
        resp.text().await
    );

    // Now fetch @test/mylib from the GROUP repo
    let resp = client
        .get(format!("{}/npm-group/@test/mylib", base_url))
        .send()
        .await
        .expect("group metadata request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "group repo should find the package in the hosted member"
    );

    let meta: Value = resp.json().await.expect("invalid json");
    assert_eq!(meta["name"], "@test/mylib");
    assert_eq!(meta["dist-tags"]["latest"], "1.0.0");
    assert!(
        meta["versions"]["1.0.0"].is_object(),
        "version 1.0.0 should be present"
    );
}

/// With a group repo [npm-private, npm-proxy]:
/// - GET a real public scoped package from npm-group
/// - The package is NOT in npm-private, so the group should fall through
///   to npm-proxy and return metadata from the upstream registry.
#[tokio::test]
async fn test_group_repo_falls_through_to_proxy() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // The package does not exist in npm-private, so the group should
    // fall through to npm-proxy and fetch from upstream.
    let resp = client
        .get(format!(
            "{}/npm-group/@{}/{}",
            base_url, PROXY_TEST_SCOPE, PROXY_TEST_NAME
        ))
        .send()
        .await
        .expect("group metadata request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "group repo should fall through to proxy for public packages"
    );

    let meta: Value = resp.json().await.expect("invalid json");

    let versions = meta
        .get("versions")
        .and_then(|v| v.as_object())
        .expect("metadata should contain versions");
    assert!(!versions.is_empty(), "versions should not be empty");

    let dist_tags = meta.get("dist-tags").and_then(|v| v.as_object());
    assert!(dist_tags.is_some(), "metadata should contain dist-tags");
}

/// Request a non-existent scoped package through the proxy.
/// The upstream registry should return a 404-level error, and our proxy
/// should propagate that as a 404.
#[tokio::test]
async fn test_proxy_handles_nonexistent_package() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{}/npm-proxy/@test/this-definitely-does-not-exist-xyz123",
            base_url
        ))
        .send()
        .await
        .expect("request for nonexistent package failed");

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "proxy should return 404 for a non-existent package"
    );
}
