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

/// Helper: create an API token for a user via the admin API.
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

/// 1. Admin creates a new repo via API, verifies it appears in list.
#[tokio::test]
async fn test_create_repository_via_api() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a new repository
    let resp = client
        .post(format!("{}/api/v1/repositories", base_url))
        .bearer_auth("test-token")
        .json(&json!({
            "name": "npm-staging",
            "type": "hosted",
            "format": "npm",
            "visibility": "private"
        }))
        .send()
        .await
        .expect("create repo request failed");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "create repository should succeed"
    );

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["name"], "npm-staging");
    assert_eq!(body["type"], "hosted");
    assert_eq!(body["format"], "npm");
    assert_eq!(body["visibility"], "private");

    // Verify it appears in GET /api/v1/repositories
    let resp = client
        .get(format!("{}/api/v1/repositories", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("list repos request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let repos: Value = resp.json().await.expect("invalid json");
    let repo_names: Vec<&str> = repos["repositories"]
        .as_array()
        .expect("repositories should be an array")
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();

    assert!(
        repo_names.contains(&"npm-staging"),
        "npm-staging should appear in repository list"
    );
    // Seeded repos should still be there
    assert!(
        repo_names.contains(&"npm-dev"),
        "npm-dev should still be in list"
    );
    assert!(
        repo_names.contains(&"npm-prod"),
        "npm-prod should still be in list"
    );

    // Verify we can get the repo details
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-staging", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let detail: Value = resp.json().await.expect("invalid json");
    assert_eq!(detail["name"], "npm-staging");
    assert_eq!(detail["visibility"], "private");
}

/// 2. Create user, set write permission on repo-A only, verify can publish
///    to repo-A but not repo-B.
#[tokio::test]
async fn test_granular_permissions() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a user with role "reader" (no write by default)
    create_user(&client, &base_url, "test-token", "dev-user", "reader").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "dev-user", "dev-token").await;
    let dev_token = token_resp["token"].as_str().expect("token should be returned");

    // Set write permission on npm-dev only
    let resp = client
        .put(format!(
            "{}/api/v1/users/dev-user/permissions/npm-dev",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "can_read": true,
            "can_write": true,
            "can_delete": false,
            "can_admin": false
        }))
        .send()
        .await
        .expect("set permission request failed");
    assert_eq!(resp.status(), StatusCode::OK, "set permission should succeed");

    // Verify permissions are listed
    let resp = client
        .get(format!("{}/api/v1/users/dev-user/permissions", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("list permissions request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let perms: Value = resp.json().await.expect("invalid json");
    let perms_array = perms["permissions"]
        .as_array()
        .expect("permissions should be an array");
    assert_eq!(perms_array.len(), 1, "should have exactly one permission entry");
    assert_eq!(perms_array[0]["repository"], "npm-dev");
    assert_eq!(perms_array[0]["can_write"], true);

    // Publish to npm-dev should succeed
    let pkg_json = r#"{"name":"@perm/test-pkg","version":"1.0.0","description":"perm test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@perm/test-pkg", "1.0.0", "perm test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@perm/test-pkg", base_url))
        .bearer_auth(dev_token)
        .json(&body)
        .send()
        .await
        .expect("publish to npm-dev should send");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reader with write permission on npm-dev should be able to publish"
    );

    // Publish to npm-prod should FAIL (no write permission)
    let pkg_json2 = r#"{"name":"@perm/test-pkg","version":"1.0.0","description":"perm test","main":"index.js"}"#;
    let tarball2 = build_tarball(pkg_json2);
    let body2 = build_publish_body("@perm/test-pkg", "1.0.0", "perm test", &tarball2);

    let resp = client
        .put(format!("{}/npm-prod/@perm/test-pkg", base_url))
        .bearer_auth(dev_token)
        .json(&body2)
        .send()
        .await
        .expect("publish to npm-prod should send");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "reader without write permission on npm-prod should be denied"
    );
}

/// 3. User with no write permission on a repo gets 403 on publish.
#[tokio::test]
async fn test_permission_denied_publish() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a reader user (no write by default)
    create_user(&client, &base_url, "test-token", "reader-only", "reader").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "reader-only", "reader-token")
            .await;
    let reader_token = token_resp["token"].as_str().expect("token should be returned");

    // Try to publish - should fail with 403
    let pkg_json = r#"{"name":"@perm/denied-pkg","version":"1.0.0","description":"denied test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@perm/denied-pkg", "1.0.0", "denied test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@perm/denied-pkg", base_url))
        .bearer_auth(reader_token)
        .json(&body)
        .send()
        .await
        .expect("publish request should send");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "reader should not be able to publish"
    );
}

/// 4. Create, list, update, delete webhooks via API.
#[tokio::test]
async fn test_crud_webhooks() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a webhook
    let resp = client
        .post(format!("{}/api/v1/webhooks", base_url))
        .bearer_auth("test-token")
        .json(&json!({
            "url": "https://example.com/hook",
            "events": ["package.published", "package.promoted"],
            "secret": "my-secret"
        }))
        .send()
        .await
        .expect("create webhook request failed");
    assert_eq!(resp.status(), StatusCode::CREATED, "create webhook should succeed");

    let created: Value = resp.json().await.expect("invalid json");
    let webhook_id = created["id"].as_i64().expect("webhook id should be present");
    assert_eq!(created["url"], "https://example.com/hook");
    let events = created["events"]
        .as_array()
        .expect("events should be an array");
    assert!(events.contains(&json!("package.published")));
    assert!(events.contains(&json!("package.promoted")));
    assert_eq!(created["active"], true);

    // List webhooks
    let resp = client
        .get(format!("{}/api/v1/webhooks", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("list webhooks request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value = resp.json().await.expect("invalid json");
    let webhooks = list["webhooks"]
        .as_array()
        .expect("webhooks should be an array");
    assert!(
        !webhooks.is_empty(),
        "should have at least one webhook"
    );

    // Update webhook
    let resp = client
        .put(format!("{}/api/v1/webhooks/{}", base_url, webhook_id))
        .bearer_auth("test-token")
        .json(&json!({
            "url": "https://example.com/hook-v2",
            "active": false
        }))
        .send()
        .await
        .expect("update webhook request failed");
    assert_eq!(resp.status(), StatusCode::OK, "update webhook should succeed");

    let updated: Value = resp.json().await.expect("invalid json");
    assert_eq!(updated["url"], "https://example.com/hook-v2");
    assert_eq!(updated["active"], false);

    // Delete webhook
    let resp = client
        .delete(format!("{}/api/v1/webhooks/{}", base_url, webhook_id))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete webhook request failed");
    assert_eq!(resp.status(), StatusCode::OK, "delete webhook should succeed");

    // Verify it's gone
    let resp = client
        .get(format!("{}/api/v1/webhooks", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("list webhooks request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value = resp.json().await.expect("invalid json");
    let webhooks = list["webhooks"]
        .as_array()
        .expect("webhooks should be an array");
    assert!(
        !webhooks.iter().any(|w| w["id"] == webhook_id),
        "deleted webhook should not appear in list"
    );
}

/// 5. Delete a repo, verify it's gone.
#[tokio::test]
async fn test_delete_repository() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // First create a repo to delete
    let resp = client
        .post(format!("{}/api/v1/repositories", base_url))
        .bearer_auth("test-token")
        .json(&json!({
            "name": "npm-to-delete",
            "type": "hosted",
            "format": "npm",
            "visibility": "public"
        }))
        .send()
        .await
        .expect("create repo request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify it exists
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-to-delete", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete it
    let resp = client
        .delete(format!("{}/api/v1/repositories/npm-to-delete", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete repo request failed");
    assert_eq!(resp.status(), StatusCode::OK, "delete should succeed");

    // Verify it's gone
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-to-delete", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "deleted repo should return 404"
    );
}
