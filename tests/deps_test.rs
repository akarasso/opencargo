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
async fn test_dependency_extraction() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // 1. Publish @test/base (no deps)
    let pkg_json = r#"{"name":"@test/base","version":"1.0.0","description":"Base package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/base", "1.0.0", "Base package", &tarball, None);

    let resp = client
        .put(format!("{}/test-npm/@test/base", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish @test/base failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. Publish @test/consumer with a dependency on @test/base
    let pkg_json2 = r#"{"name":"@test/consumer","version":"1.0.0","description":"Consumer package","main":"index.js"}"#;
    let tarball2 = build_tarball(pkg_json2);
    let deps = json!({
        "dependencies": {
            "@test/base": "^1.0.0"
        },
        "devDependencies": {
            "jest": "^29.0.0"
        }
    });
    let body2 = build_publish_body(
        "@test/consumer",
        "1.0.0",
        "Consumer package",
        &tarball2,
        Some(deps),
    );

    let resp = client
        .put(format!("{}/test-npm/@test/consumer", base_url))
        .bearer_auth("test-token")
        .json(&body2)
        .send()
        .await
        .expect("publish @test/consumer failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. GET dependencies of @test/consumer
    let resp = client
        .get(format!(
            "{}/api/v1/deps/@test/consumer/dependencies",
            base_url
        ))
        .send()
        .await
        .expect("get dependencies failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let deps_response: Value = resp.json().await.expect("invalid json");
    assert_eq!(deps_response["package"], "@test/consumer");
    assert_eq!(deps_response["version"], "1.0.0");

    let dep_list = deps_response["dependencies"]
        .as_array()
        .expect("dependencies should be an array");

    // Should have 2 dependencies: @test/base (runtime) and jest (dev)
    assert_eq!(dep_list.len(), 2, "should have 2 dependencies");

    let base_dep = dep_list
        .iter()
        .find(|d| d["name"] == "@test/base")
        .expect("should find @test/base in dependencies");
    assert_eq!(base_dep["version_req"], "^1.0.0");
    assert_eq!(base_dep["type"], "runtime");

    let jest_dep = dep_list
        .iter()
        .find(|d| d["name"] == "jest")
        .expect("should find jest in dependencies");
    assert_eq!(jest_dep["version_req"], "^29.0.0");
    assert_eq!(jest_dep["type"], "dev");
}

#[tokio::test]
async fn test_dependents() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // 1. Publish @test/base
    let pkg_json = r#"{"name":"@test/base","version":"1.0.0","description":"Base package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/base", "1.0.0", "Base package", &tarball, None);

    let resp = client
        .put(format!("{}/test-npm/@test/base", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. Publish @test/consumer with dep on @test/base
    let pkg_json2 = r#"{"name":"@test/consumer","version":"1.0.0","description":"Consumer","main":"index.js"}"#;
    let tarball2 = build_tarball(pkg_json2);
    let deps = json!({
        "dependencies": {
            "@test/base": "^1.0.0"
        }
    });
    let body2 = build_publish_body(
        "@test/consumer",
        "1.0.0",
        "Consumer",
        &tarball2,
        Some(deps),
    );

    let resp = client
        .put(format!("{}/test-npm/@test/consumer", base_url))
        .bearer_auth("test-token")
        .json(&body2)
        .send()
        .await
        .expect("publish consumer failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. GET dependents of @test/base
    let resp = client
        .get(format!(
            "{}/api/v1/deps/@test/base/dependents",
            base_url
        ))
        .send()
        .await
        .expect("get dependents failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let dependents_response: Value = resp.json().await.expect("invalid json");
    assert_eq!(dependents_response["package"], "@test/base");

    let dependents = dependents_response["dependents"]
        .as_array()
        .expect("dependents should be an array");
    assert!(!dependents.is_empty(), "should have at least one dependent");

    let consumer = dependents
        .iter()
        .find(|d| d["name"] == "@test/consumer")
        .expect("@test/consumer should be a dependent");
    assert_eq!(consumer["version"], "1.0.0");
}

#[tokio::test]
async fn test_impact_analysis() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // 1. Publish @test/base
    let pkg_json = r#"{"name":"@test/base","version":"1.0.0","description":"Base package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/base", "1.0.0", "Base package", &tarball, None);

    let resp = client
        .put(format!("{}/test-npm/@test/base", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. Publish @test/consumer with dep on @test/base
    let pkg_json2 = r#"{"name":"@test/consumer","version":"1.0.0","description":"Consumer","main":"index.js"}"#;
    let tarball2 = build_tarball(pkg_json2);
    let deps = json!({
        "dependencies": {
            "@test/base": "^1.0.0"
        }
    });
    let body2 = build_publish_body(
        "@test/consumer",
        "1.0.0",
        "Consumer",
        &tarball2,
        Some(deps),
    );

    let resp = client
        .put(format!("{}/test-npm/@test/consumer", base_url))
        .bearer_auth("test-token")
        .json(&body2)
        .send()
        .await
        .expect("publish consumer failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. GET impact of @test/base version 1.0.0 (requires auth)
    let resp = client
        .get(format!(
            "{}/api/v1/deps/@test/base/versions/1.0.0/impact",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("impact analysis failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let impact: Value = resp.json().await.expect("invalid json");
    assert_eq!(impact["package"], "@test/base");
    assert_eq!(impact["version"], "1.0.0");
    assert_eq!(impact["safe_to_delete"], false);

    let affected = impact["affected_packages"]
        .as_array()
        .expect("affected_packages should be an array");
    assert!(
        affected.iter().any(|p| p == "@test/consumer"),
        "affected packages should include @test/consumer"
    );
}
