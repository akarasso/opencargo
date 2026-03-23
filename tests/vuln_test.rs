use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AdminConfig, AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility, VulnScanConfig,
};
use opencargo::server;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn build_publish_body(
    package_name: &str,
    version: &str,
    description: &str,
    tarball_data: &[u8],
    deps: Option<Value>,
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(tarball_data);
    let attachment_key = format!(
        "{}-{}.tgz",
        package_name.split('/').last().unwrap_or(package_name),
        version
    );

    let mut version_meta = json!({
        "name": package_name,
        "version": version,
        "description": description,
        "main": "index.js",
        "dist": {
            "shasum": ""
        }
    });

    if let Some(d) = deps {
        version_meta
            .as_object_mut()
            .unwrap()
            .insert("dependencies".to_string(), d);
    }

    json!({
        "name": package_name,
        "description": description,
        "dist-tags": { "latest": version },
        "versions": {
            version: version_meta
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

async fn setup_with_vuln_scan(vuln_config: VulnScanConfig) -> (String, tokio::task::JoinHandle<()>, TempDir) {
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
            admin: AdminConfig {
                username: "admin".to_string(),
                password: String::new(),
            },
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
        vuln_scan: vuln_config,
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

    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client
            .get(format!("{}/health/live", &base_url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test that publishing a package with dependencies triggers a vulnerability
/// scan and that the scan results are available via the API.
#[tokio::test]
async fn test_vuln_scan_on_publish() {
    let (base_url, _handle, _tmp) = setup_with_vuln_scan(VulnScanConfig {
        enabled: true,
        block_on_critical: false,
    })
    .await;

    let client = reqwest::Client::new();

    // Publish a package with a dependency (lodash 4.17.20 had known vulns)
    let pkg_json = r#"{"name":"@test/vuln-pkg","version":"1.0.0","description":"Vuln test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body(
        "@test/vuln-pkg",
        "1.0.0",
        "Vuln test",
        &tarball,
        Some(json!({"lodash": "4.17.20"})),
    );

    let resp = client
        .put(format!("{}/test-npm/@test/vuln-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // Wait for the background scan to complete
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Check the vuln scan results via API
    let resp = client
        .get(format!(
            "{}/api/v1/vulns/@test/vuln-pkg/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("vulns request failed");

    assert_eq!(resp.status(), StatusCode::OK, "vulns endpoint should succeed");

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["package"], "@test/vuln-pkg");
    assert_eq!(body["version"], "1.0.0");

    // The scan should have been performed — check that scanned_at is not null
    // (We don't assert specific vulns since osv.dev results may change)
    assert!(
        body["scanned_at"].as_str().is_some(),
        "scan should have been performed (scanned_at should be set). Got: {}",
        body
    );
    assert!(
        body["total_deps"].as_i64().unwrap_or(-1) >= 0,
        "total_deps should be returned"
    );
}

/// Test that a package with no dependencies gets a clean scan.
#[tokio::test]
async fn test_vuln_scan_clean_package() {
    let (base_url, _handle, _tmp) = setup_with_vuln_scan(VulnScanConfig {
        enabled: true,
        block_on_critical: false,
    })
    .await;

    let client = reqwest::Client::new();

    // Publish a package with NO dependencies
    let pkg_json =
        r#"{"name":"@test/clean-pkg","version":"1.0.0","description":"Clean test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/clean-pkg", "1.0.0", "Clean test", &tarball, None);

    let resp = client
        .put(format!("{}/test-npm/@test/clean-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // Wait for background scan
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Check the vuln scan results
    let resp = client
        .get(format!(
            "{}/api/v1/vulns/@test/clean-pkg/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("vulns request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["package"], "@test/clean-pkg");
    assert_eq!(body["version"], "1.0.0");
    assert_eq!(body["total_deps"], 0);
    assert_eq!(body["vulnerable_deps"], 0);
    assert_eq!(body["status"], "clean");
}
