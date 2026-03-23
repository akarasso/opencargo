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
// Helpers (same pattern as npm_test.rs)
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

/// Helper: create a user via the admin API.
/// Returns the full response body which now includes a `password` field.
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

/// Helper: change a user's password via the API, then return the new password.
/// Uses admin token to skip current_password check.
async fn change_password_as_admin(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    new_password: &str,
) {
    let resp = client
        .put(format!("{}/api/v1/users/{}/password", base_url, username))
        .bearer_auth(admin_token)
        .json(&json!({
            "new_password": new_password
        }))
        .send()
        .await
        .expect("change password request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from change password");
    assert_eq!(
        status,
        StatusCode::OK,
        "change password failed: {:?}",
        body
    );
    assert_eq!(body["ok"], true);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Static token from config should still work for publish and read (backwards compat).
#[tokio::test]
async fn test_static_token_still_works() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish a package using the static token
    let pkg_json = r#"{"name":"@auth/static-test","version":"1.0.0","description":"static token test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@auth/static-test", "1.0.0", "static token test", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@auth/static-test", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish with static token should succeed");

    // Install (GET metadata) using anonymous read (no token needed since anonymous_read=true)
    let resp = client
        .get(format!("{}/test-npm/@auth/static-test", base_url))
        .send()
        .await
        .expect("get metadata request failed");
    assert_eq!(resp.status(), StatusCode::OK, "anonymous read should work");

    let meta: Value = resp.json().await.expect("invalid json");
    assert_eq!(meta["name"], "@auth/static-test");

    // Also verify whoami with static token
    let resp = client
        .get(format!("{}/-/whoami", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("whoami request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let whoami: Value = resp.json().await.expect("invalid json");
    assert_eq!(whoami["username"], "static-token");
}

/// 2. Create a user and verify it appears in the list. Response includes generated password.
#[tokio::test]
async fn test_create_user_and_list() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "testdev" with role "publisher"
    let created = create_user(&client, &base_url, "test-token", "testdev", "publisher").await;
    assert_eq!(created["username"], "testdev");
    assert_eq!(created["role"], "publisher");

    // Generated password must be present and non-empty
    let generated_password = created["password"]
        .as_str()
        .expect("password should be present in create response");
    assert!(
        !generated_password.is_empty(),
        "generated password should not be empty"
    );
    assert!(
        generated_password.len() >= 20,
        "generated password should be at least 20 chars"
    );

    // Password hash must NOT appear in the create response
    assert!(
        created.get("password_hash").is_none(),
        "password_hash should not be in create response"
    );

    // List users
    let resp = client
        .get(format!("{}/api/v1/users", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("list users request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let users: Value = resp.json().await.expect("invalid json");
    let users_array = users.as_array().expect("users should be an array");

    // Find "testdev" in the list
    let testdev = users_array
        .iter()
        .find(|u| u["username"] == "testdev")
        .expect("testdev should be in the users list");

    assert_eq!(testdev["role"], "publisher");

    // Password hash must NOT appear in the list response
    assert!(
        testdev.get("password_hash").is_none(),
        "password_hash should not be in list response"
    );
    // Password should NOT appear in list response either (only in create)
    assert!(
        testdev.get("password").is_none(),
        "password should not be in list response"
    );
}

/// 3. Create a token for a user and verify /-/whoami returns that user's name.
#[tokio::test]
async fn test_create_token_and_use_it() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "tokenuser"
    create_user(&client, &base_url, "test-token", "tokenuser", "publisher").await;

    // Create a token for "tokenuser"
    let token_resp = create_token_for_user(&client, &base_url, "test-token", "tokenuser", "my-token").await;
    let raw_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");
    assert!(
        raw_token.starts_with("trg_"),
        "token should start with trg_ prefix"
    );

    // Use the token to call /-/whoami
    let resp = client
        .get(format!("{}/-/whoami", base_url))
        .bearer_auth(raw_token)
        .send()
        .await
        .expect("whoami request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let whoami: Value = resp.json().await.expect("invalid json");
    assert_eq!(whoami["username"], "tokenuser");
}

/// 4. Publish a package using an API token created for a publisher user.
#[tokio::test]
async fn test_publish_with_api_token() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "publisher1" with role "publisher"
    create_user(&client, &base_url, "test-token", "publisher1", "publisher").await;

    // Create a token for "publisher1"
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "publisher1", "pub-token").await;
    let raw_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    // Publish a package using that token
    let pkg_json = r#"{"name":"@auth/pub-test","version":"1.0.0","description":"publisher test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@auth/pub-test", "1.0.0", "publisher test", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@auth/pub-test", base_url))
        .bearer_auth(raw_token)
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "publish with publisher token should succeed"
    );
}

/// 5. A reader should NOT be able to publish packages.
#[tokio::test]
async fn test_reader_cannot_publish() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "reader1" with role "reader"
    create_user(&client, &base_url, "test-token", "reader1", "reader").await;

    // Create a token for "reader1"
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "reader1", "read-token").await;
    let raw_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    // Try to publish with the reader token
    let pkg_json = r#"{"name":"@auth/reader-test","version":"1.0.0","description":"reader test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@auth/reader-test", "1.0.0", "reader test", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@auth/reader-test", base_url))
        .bearer_auth(raw_token)
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::UNAUTHORIZED,
        "reader should not be allowed to publish, got status: {}",
        resp.status()
    );
}

/// 6. npm login: create user, change password, then login with new password.
#[tokio::test]
async fn test_npm_login() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "loginuser" — password is generated server-side
    let created = create_user(&client, &base_url, "test-token", "loginuser", "publisher").await;
    let _generated_password = created["password"]
        .as_str()
        .expect("password should be returned");

    // Change the password (as admin) so must_change_password is cleared
    let new_password = "mypassword";
    change_password_as_admin(&client, &base_url, "test-token", "loginuser", new_password).await;

    // PUT /-/user/org.couchdb.user:loginuser with name + password
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:loginuser", base_url))
        .json(&json!({
            "name": "loginuser",
            "password": new_password
        }))
        .send()
        .await
        .expect("npm login request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from npm login");
    assert_eq!(
        status,
        StatusCode::CREATED,
        "npm login should succeed, got: {:?}",
        body
    );
    assert_eq!(body["ok"], true);

    let login_token = body["token"]
        .as_str()
        .expect("login should return a token");

    assert!(
        login_token.starts_with("trg_"),
        "login token should start with trg_ prefix"
    );

    // Use the token to call /-/whoami
    let resp = client
        .get(format!("{}/-/whoami", base_url))
        .bearer_auth(login_token)
        .send()
        .await
        .expect("whoami request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let whoami: Value = resp.json().await.expect("invalid json");
    assert_eq!(whoami["username"], "loginuser");
}

/// 7. npm login with wrong password must return 401.
#[tokio::test]
async fn test_npm_login_wrong_password() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "loginuser" and change their password
    create_user(&client, &base_url, "test-token", "loginuser", "publisher").await;
    change_password_as_admin(&client, &base_url, "test-token", "loginuser", "correct-password").await;

    // Attempt login with wrong password
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:loginuser", base_url))
        .json(&json!({
            "name": "loginuser",
            "password": "wrong-password"
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "npm login with wrong password should return 401"
    );
}

/// 8. Revoking a token prevents further use.
#[tokio::test]
async fn test_revoke_token() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user and token
    create_user(&client, &base_url, "test-token", "revokeuser", "publisher").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "revokeuser", "revoke-token").await;
    let raw_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");
    let token_id = token_resp["id"]
        .as_str()
        .expect("token id should be returned");

    // Use the token — should work
    let resp = client
        .get(format!("{}/-/whoami", base_url))
        .bearer_auth(raw_token)
        .send()
        .await
        .expect("whoami request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let whoami: Value = resp.json().await.expect("invalid json");
    assert_eq!(whoami["username"], "revokeuser");

    // Revoke the token via admin API
    let resp = client
        .delete(format!(
            "{}/api/v1/users/revokeuser/tokens/{}",
            base_url, token_id
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete token request failed");
    assert_eq!(resp.status(), StatusCode::OK, "revoke should succeed");

    // Try to use the revoked token — should fail
    let resp = client
        .get(format!("{}/-/whoami", base_url))
        .bearer_auth(raw_token)
        .send()
        .await
        .expect("whoami request failed after revoke");

    // With anonymous_read=true, a GET with an invalid token still passes through
    // as anonymous, returning "anonymous" for whoami.
    let whoami: Value = resp.json().await.expect("invalid json");
    assert_eq!(
        whoami["username"], "anonymous",
        "revoked token should not authenticate; should fall back to anonymous"
    );
}

/// 9. A non-admin user cannot create users.
#[tokio::test]
async fn test_non_admin_cannot_create_users() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a publisher user and get a token for them
    create_user(&client, &base_url, "test-token", "normaluser", "publisher").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "normaluser", "normal-token").await;
    let raw_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    // Try to create a user using the publisher's token
    let resp = client
        .post(format!("{}/api/v1/users", base_url))
        .bearer_auth(raw_token)
        .json(&json!({
            "username": "sneaky",
            "role": "admin"
        }))
        .send()
        .await
        .expect("create user request failed");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "non-admin should not be able to create users"
    );
}

/// 10. Audit log endpoint returns entries (admin only).
#[tokio::test]
async fn test_audit_log() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Perform some actions: create user, publish a package
    create_user(&client, &base_url, "test-token", "audituser", "publisher").await;

    let pkg_json = r#"{"name":"@auth/audit-test","version":"1.0.0","description":"audit test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@auth/audit-test", "1.0.0", "audit test", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@auth/audit-test", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Query audit log as admin
    let resp = client
        .get(format!("{}/api/v1/system/audit", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("audit request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let audit_body: Value = resp.json().await.expect("invalid json from audit");
    assert!(
        audit_body.get("entries").is_some(),
        "audit response should have an 'entries' field"
    );
    assert!(
        audit_body.get("page").is_some(),
        "audit response should have a 'page' field"
    );

    // Verify a non-admin cannot access the audit log
    create_user(&client, &base_url, "test-token", "nonadmin", "reader").await;
    let token_resp =
        create_token_for_user(&client, &base_url, "test-token", "nonadmin", "nonadmin-token").await;
    let non_admin_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    let resp = client
        .get(format!("{}/api/v1/system/audit", base_url))
        .bearer_auth(non_admin_token)
        .send()
        .await
        .expect("audit request failed");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "non-admin should not access audit log"
    );
}

/// 11. Password change: create user, get generated password, change via endpoint, login with new password.
#[tokio::test]
async fn test_password_change() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user "pwuser" — gets a generated password
    let created = create_user(&client, &base_url, "test-token", "pwuser", "publisher").await;
    let generated_password = created["password"]
        .as_str()
        .expect("generated password should be present");

    // Create a token for "pwuser" so they can call the change-password endpoint
    let token_resp = create_token_for_user(&client, &base_url, "test-token", "pwuser", "pw-token").await;
    let user_token = token_resp["token"]
        .as_str()
        .expect("token should be returned");

    // Change password using the generated password as current_password
    let new_password = "my-new-secure-password";
    let resp = client
        .put(format!("{}/api/v1/users/pwuser/password", base_url))
        .bearer_auth(user_token)
        .json(&json!({
            "current_password": generated_password,
            "new_password": new_password
        }))
        .send()
        .await
        .expect("change password request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(status, StatusCode::OK, "password change should succeed: {:?}", body);
    assert_eq!(body["ok"], true);

    // Now npm login with the NEW password should work
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:pwuser", base_url))
        .json(&json!({
            "name": "pwuser",
            "password": new_password
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "npm login with new password should succeed"
    );

    // Login with old password should fail
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:pwuser", base_url))
        .json(&json!({
            "name": "pwuser",
            "password": generated_password
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "npm login with old password should fail"
    );
}

/// 12. must_change_password blocks npm login for initial admin until password is changed.
#[tokio::test]
async fn test_must_change_password_blocks_npm_login() {
    let (base_url, _handle, tmp) = setup().await;
    let client = reqwest::Client::new();

    // The initial admin has must_change_password = 1
    // Read the generated password from the admin.password file
    let password_file = tmp.path().join("admin.password");
    let admin_password = std::fs::read_to_string(&password_file)
        .expect("admin.password file should exist");

    // Try npm login with the initial admin password — should succeed but flag must_change_password
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:admin", base_url))
        .json(&json!({
            "name": "admin",
            "password": admin_password.trim()
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "npm login should succeed even with must_change_password"
    );
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["must_change_password"], true,
        "login response should flag must_change_password"
    );

    // Change the password via the password change endpoint (using static admin token)
    let new_password = "changed-admin-password";
    change_password_as_admin(&client, &base_url, "test-token", "admin", new_password).await;

    // Now npm login should return must_change_password: false
    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:admin", base_url))
        .json(&json!({
            "name": "admin",
            "password": new_password
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "npm login should succeed after password change"
    );
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["must_change_password"], false,
        "after password change, must_change_password should be false"
    );

    // A user created via API should NOT have must_change_password
    let created = create_user(&client, &base_url, "test-token", "apiuser", "publisher").await;
    let api_password = created["password"].as_str().expect("password present");

    let resp = client
        .put(format!("{}/-/user/org.couchdb.user:apiuser", base_url))
        .json(&json!({
            "name": "apiuser",
            "password": api_password
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "API-created user should login without forced password change"
    );
}

/// 13. Rate limiting: npm login should be rate limited to 5 attempts per minute.
#[tokio::test]
async fn test_rate_limit_npm_login() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create user and set password
    create_user(&client, &base_url, "test-token", "ratelimituser", "publisher").await;
    change_password_as_admin(&client, &base_url, "test-token", "ratelimituser", "testpass").await;

    // Make 5 login attempts (these should all succeed or return auth errors, but NOT 429)
    for i in 0..5 {
        let resp = client
            .put(format!(
                "{}/-/user/org.couchdb.user:ratelimituser",
                base_url
            ))
            .json(&json!({
                "name": "ratelimituser",
                "password": "wrong-password"
            }))
            .send()
            .await
            .expect("npm login request failed");

        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {} should not be rate limited",
            i + 1
        );
    }

    // The 6th attempt should be rate limited (429)
    let resp = client
        .put(format!(
            "{}/-/user/org.couchdb.user:ratelimituser",
            base_url
        ))
        .json(&json!({
            "name": "ratelimituser",
            "password": "wrong-password"
        }))
        .send()
        .await
        .expect("npm login request failed");

    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "6th login attempt should return 429 Too Many Requests"
    );
}
