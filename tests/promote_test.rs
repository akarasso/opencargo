use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AdminConfig, AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility,
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

/// Start a test server with two npm hosted repos (npm-dev and npm-prod) and one cargo repo.
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
            admin: AdminConfig {
                username: "admin".to_string(),
                password: String::new(),
            },
            ..Default::default()
        },
        repositories: vec![
            RepositoryConfig {
                name: "npm-dev".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "npm-prod".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "cargo-dev".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Cargo,
                visibility: Visibility::Public,
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

    // Wait for the server to be ready
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{}/health/live", &base_url)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

/// Helper: create a user via the admin API.
async fn create_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    role: &str,
) -> Value {
    let resp = client
        .post(format!("{}/api/v1/users", base_url))
        .bearer_auth(admin_token)
        .json(&json!({
            "username": username,
            "role": role
        }))
        .send()
        .await
        .expect("create user request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from create user");
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create user failed: {:?}",
        body
    );
    body
}

/// Helper: create an API token for a user.
async fn create_token_for_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    token_name: &str,
) -> Value {
    let resp = client
        .post(format!("{}/api/v1/users/{}/tokens", base_url, username))
        .bearer_auth(admin_token)
        .json(&json!({
            "name": token_name,
        }))
        .send()
        .await
        .expect("create token request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from create token");
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create token failed: {:?}",
        body
    );
    body
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test the full promotion flow: publish to npm-dev, promote to npm-prod,
/// verify package exists in npm-prod and tarball is downloadable.
#[tokio::test]
async fn test_promote_package() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // 1. Publish @test/promote-me@1.0.0 to npm-dev
    let pkg_json = r#"{"name":"@test/promote-me","version":"1.0.0","description":"promote test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/promote-me", "1.0.0", "promote test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/promote-me", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // 2. Promote from npm-dev to npm-prod
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/promote-me/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("promote request failed");

    let status = resp.status();
    let promote_body: Value = resp.json().await.expect("invalid json from promote");
    assert_eq!(status, StatusCode::OK, "promote should succeed: {:?}", promote_body);
    assert_eq!(promote_body["ok"], true);
    assert_eq!(promote_body["package"], "@test/promote-me");
    assert_eq!(promote_body["version"], "1.0.0");
    assert_eq!(promote_body["from"], "npm-dev");
    assert_eq!(promote_body["to"], "npm-prod");

    // 3. GET @test/promote-me from npm-prod — assert it exists with version 1.0.0
    let resp = client
        .get(format!("{}/npm-prod/@test/promote-me", base_url))
        .send()
        .await
        .expect("get metadata request failed");
    assert_eq!(resp.status(), StatusCode::OK, "package should exist in npm-prod");

    let meta: Value = resp.json().await.expect("invalid json");
    assert_eq!(meta["name"], "@test/promote-me");
    assert!(
        meta["versions"]["1.0.0"].is_object(),
        "version 1.0.0 should exist in npm-prod"
    );
    assert_eq!(
        meta["dist-tags"]["latest"], "1.0.0",
        "latest tag should be promoted too"
    );

    // Verify tarball URL points to npm-prod
    let tarball_url = meta["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .expect("tarball url should be present");
    assert!(
        tarball_url.contains("/npm-prod/"),
        "tarball URL should reference npm-prod repo, got: {}",
        tarball_url
    );

    // 4. Download tarball from npm-prod — assert it works (same data)
    let resp = client
        .get(format!(
            "{}/npm-prod/@test/promote-me/-/promote-me-1.0.0.tgz",
            base_url
        ))
        .send()
        .await
        .expect("download request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tarball download from npm-prod should work"
    );

    let downloaded = resp.bytes().await.expect("failed to read tarball bytes");
    assert_eq!(
        downloaded.as_ref(),
        tarball.as_slice(),
        "downloaded tarball should match the original"
    );
}

/// Promoting the same version twice should return 409 Conflict.
#[tokio::test]
async fn test_promote_duplicate_version() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish
    let pkg_json = r#"{"name":"@test/dup-promote","version":"2.0.0","description":"dup test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/dup-promote", "2.0.0", "dup test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/dup-promote", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // First promote — should succeed
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/dup-promote/2.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("first promote request failed");
    assert_eq!(resp.status(), StatusCode::OK, "first promote should succeed");

    // Second promote — should fail with 409
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/dup-promote/2.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("second promote request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "second promote should return 409 Conflict"
    );
}

/// A publisher user should not be able to promote (requires admin).
#[tokio::test]
async fn test_promote_requires_admin() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a publisher user and get a token
    create_user(&client, &base_url, "test-token", "publisher1", "publisher").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "publisher1", "pub-token").await;
    let pub_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    // Publish a package with the publisher token
    let pkg_json = r#"{"name":"@test/perm-test","version":"1.0.0","description":"perm test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/perm-test", "1.0.0", "perm test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/perm-test", base_url))
        .bearer_auth(pub_token)
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // Try to promote with the publisher token — should get 403
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/perm-test/1.0.0",
            base_url
        ))
        .bearer_auth(pub_token)
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("promote request failed");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "publisher should not be able to promote"
    );
}

/// Promoting from an npm repo to a cargo repo should fail with 400.
#[tokio::test]
async fn test_promote_cross_format_fails() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish a package to npm-dev
    let pkg_json = r#"{"name":"@test/cross-fmt","version":"1.0.0","description":"cross format test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/cross-fmt", "1.0.0", "cross format test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/cross-fmt", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // Try to promote from npm-dev to cargo-dev — should fail with 400
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/cross-fmt/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "cargo-dev"
        }))
        .send()
        .await
        .expect("promote request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "cross-format promotion should return 400"
    );
}

/// Verify the promotions history endpoint returns promotion records.
#[tokio::test]
async fn test_list_promotions() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish and promote
    let pkg_json = r#"{"name":"@test/history","version":"3.0.0","description":"history test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/history", "3.0.0", "history test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/history", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/history/3.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("promote request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Query promotions history
    let resp = client
        .get(format!(
            "{}/api/v1/promotions/@test/history/3.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("promotions request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let history: Value = resp.json().await.expect("invalid json from promotions");
    assert_eq!(history["package"], "@test/history");
    assert_eq!(history["version"], "3.0.0");

    let promotions = history["promotions"]
        .as_array()
        .expect("promotions should be an array");
    assert_eq!(promotions.len(), 1, "should have exactly one promotion entry");
    assert_eq!(promotions[0]["from"], "npm-dev");
    assert_eq!(promotions[0]["to"], "npm-prod");
    assert!(
        promotions[0]["promoted_by"].as_str().is_some(),
        "promoted_by should be present"
    );
    assert!(
        promotions[0]["promoted_at"].as_str().is_some(),
        "promoted_at should be present"
    );
}
