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

    if let Some(deps_val) = deps {
        if let Some(obj) = version_meta.as_object_mut() {
            if let Some(d) = deps_val.as_object() {
                for (key, val) in d {
                    obj.insert(key.clone(), val.clone());
                }
            }
        }
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

/// Start a test server with two npm hosted repos for promotion testing.
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
                name: "test-npm-dev".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "test-npm-prod".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
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

/// Publish @test/scoped-pkg@1.0.0 with dependencies to the test-npm-dev repo.
async fn publish_scoped_package(client: &reqwest::Client, base_url: &str) {
    let pkg_json = r#"{"name":"@test/scoped-pkg","version":"1.0.0","description":"Scoped E2E test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let deps = json!({
        "dependencies": {
            "lodash": "^4.17.21"
        },
        "devDependencies": {
            "typescript": "^5.0.0"
        }
    });
    let body = build_publish_body(
        "@test/scoped-pkg",
        "1.0.0",
        "Scoped E2E test package",
        &tarball,
        Some(deps),
    );

    let resp = client
        .put(format!("{}/test-npm-dev/@test/scoped-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "publish @test/scoped-pkg should succeed: {:?}",
        resp.text().await
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1: GET /{repo}/@test/scoped-pkg returns 200 with valid metadata.
#[tokio::test]
async fn test_scoped_package_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .get(format!("{}/test-npm-dev/@test/scoped-pkg", base_url))
        .send()
        .await
        .expect("get metadata request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let meta: Value = resp.json().await.expect("invalid json");

    assert_eq!(meta["name"], "@test/scoped-pkg");
    assert_eq!(meta["dist-tags"]["latest"], "1.0.0");
    assert!(
        meta["versions"]["1.0.0"].is_object(),
        "version 1.0.0 should exist"
    );
    assert_eq!(meta["versions"]["1.0.0"]["version"], "1.0.0");
}

/// Test 2: GET /api/v1/vulns/@test/scoped-pkg/1.0.0 returns 200 (with auth).
#[tokio::test]
async fn test_scoped_package_vulns() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    // Allow time for background scan
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let resp = client
        .get(format!(
            "{}/api/v1/vulns/@test/scoped-pkg/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("vulns request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vulns endpoint should succeed for scoped package"
    );

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["package"], "@test/scoped-pkg");
    assert_eq!(body["version"], "1.0.0");
}

/// Test 3: GET /api/v1/deps/@test/scoped-pkg/dependencies returns 200.
#[tokio::test]
async fn test_scoped_package_deps() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .get(format!(
            "{}/api/v1/deps/@test/scoped-pkg/dependencies",
            base_url
        ))
        .send()
        .await
        .expect("get dependencies failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dependencies endpoint should succeed for scoped package"
    );

    let deps_response: Value = resp.json().await.expect("invalid json");
    assert_eq!(deps_response["package"], "@test/scoped-pkg");
    assert_eq!(deps_response["version"], "1.0.0");

    let dep_list = deps_response["dependencies"]
        .as_array()
        .expect("dependencies should be an array");

    // Should have 2 dependencies: lodash (runtime) and typescript (dev)
    assert_eq!(dep_list.len(), 2, "should have 2 dependencies");

    let lodash_dep = dep_list
        .iter()
        .find(|d| d["name"] == "lodash")
        .expect("should find lodash in dependencies");
    assert_eq!(lodash_dep["version_req"], "^4.17.21");
    assert_eq!(lodash_dep["type"], "runtime");

    let ts_dep = dep_list
        .iter()
        .find(|d| d["name"] == "typescript")
        .expect("should find typescript in dependencies");
    assert_eq!(ts_dep["version_req"], "^5.0.0");
    assert_eq!(ts_dep["type"], "dev");
}

/// Test 4: GET /api/v1/deps/@test/scoped-pkg/dependents returns 200.
#[tokio::test]
async fn test_scoped_package_dependents() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .get(format!(
            "{}/api/v1/deps/@test/scoped-pkg/dependents",
            base_url
        ))
        .send()
        .await
        .expect("get dependents failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dependents endpoint should succeed for scoped package"
    );

    let dependents_response: Value = resp.json().await.expect("invalid json");
    assert_eq!(dependents_response["package"], "@test/scoped-pkg");

    // No dependents expected yet (nothing depends on this package)
    let dependents = dependents_response["dependents"]
        .as_array()
        .expect("dependents should be an array");
    assert_eq!(dependents.len(), 0, "no dependents expected");
}

/// Test 5: POST /api/v1/promote/@test/scoped-pkg/1.0.0 returns 200 (promote from test-npm-dev to test-npm-prod).
#[tokio::test]
async fn test_scoped_package_promote() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/scoped-pkg/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "test-npm-dev",
            "to": "test-npm-prod"
        }))
        .send()
        .await
        .expect("promote request failed");

    let status = resp.status();
    let promote_body: Value = resp.json().await.expect("invalid json from promote");
    assert_eq!(
        status,
        StatusCode::OK,
        "promote should succeed: {:?}",
        promote_body
    );
    assert_eq!(promote_body["ok"], true);
    assert_eq!(promote_body["package"], "@test/scoped-pkg");
    assert_eq!(promote_body["version"], "1.0.0");
    assert_eq!(promote_body["from"], "test-npm-dev");
    assert_eq!(promote_body["to"], "test-npm-prod");

    // Verify the package exists in the target repo
    let resp = client
        .get(format!("{}/test-npm-prod/@test/scoped-pkg", base_url))
        .send()
        .await
        .expect("get metadata from prod failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "promoted package should exist in test-npm-prod"
    );

    let meta: Value = resp.json().await.expect("invalid json");
    assert_eq!(meta["name"], "@test/scoped-pkg");
    assert!(
        meta["versions"]["1.0.0"].is_object(),
        "version 1.0.0 should exist in test-npm-prod"
    );
}

/// Test 6: GET /{repo}/-/v1/search?text=scoped returns results containing the package.
#[tokio::test]
async fn test_scoped_package_search() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .get(format!(
            "{}/test-npm-dev/-/v1/search?text=scoped",
            base_url
        ))
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
        .any(|o| o["package"]["name"].as_str() == Some("@test/scoped-pkg"));
    assert!(
        found,
        "search results should contain @test/scoped-pkg"
    );
}

/// Test 7: GET /api/v1/packages/@test/scoped-pkg returns 200 with package detail.
#[tokio::test]
async fn test_scoped_package_dashboard_api() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    publish_scoped_package(&client, &base_url).await;

    let resp = client
        .get(format!(
            "{}/api/v1/packages/@test/scoped-pkg",
            base_url
        ))
        .send()
        .await
        .expect("dashboard API request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dashboard package detail should succeed for scoped package"
    );

    let detail: Value = resp.json().await.expect("invalid json");
    assert_eq!(detail["name"], "@test/scoped-pkg");
    assert_eq!(detail["description"], "Scoped E2E test package");

    let versions = detail["versions"]
        .as_array()
        .expect("versions should be an array");
    assert!(
        !versions.is_empty(),
        "should have at least one version"
    );
}
