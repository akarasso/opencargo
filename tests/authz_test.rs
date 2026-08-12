//! Authorization regression tests for the vulns API (S-audit item 1) and
//! `GET /api/v1/repositories/{name}` (S-audit item 2).
//!
//! Policy under test:
//! - a package hosted in a repository the caller cannot read must be
//!   indistinguishable from a package that does not exist (404, never 401/403);
//! - rescan additionally requires write access on the repo (403 when the
//!   caller can read but not write), because it destroys stored scan results;
//! - repository details (which include `upstream_url`/`config_json`) require
//!   read access; denial is the same 404 as a missing repository.

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
) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(tarball_data);
    let attachment_key = format!(
        "{}-{}.tgz",
        package_name.split('/').next_back().unwrap_or(package_name),
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

/// Start a server with one public repo (`npm-pub`) and one private repo
/// (`npm-secret`). The vuln scanner is disabled so rescans never reach the
/// network — access-control outcomes are what these tests assert on.
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
                name: "npm-pub".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "npm-secret".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Private,
                upstream: None,
                members: None,
            },
        ],
        vuln_scan: VulnScanConfig {
            enabled: false,
            block_on_critical: false,
        },
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
    assert_eq!(status, StatusCode::CREATED, "create user failed: {:?}", body);
    body
}

/// Helper: create an API token for a user via the admin API.
async fn create_token_for_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    token_name: &str,
) -> String {
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
    assert_eq!(status, StatusCode::CREATED, "create token failed: {:?}", body);
    body["token"]
        .as_str()
        .expect("token should be returned")
        .to_string()
}

/// Helper: set an explicit permission row for a user on a repository.
async fn set_permission(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    repo: &str,
    perms: Value,
) {
    let resp = client
        .put(format!(
            "{}/api/v1/users/{}/permissions/{}",
            base_url, username, repo
        ))
        .bearer_auth(admin_token)
        .json(&perms)
        .send()
        .await
        .expect("set permission request failed");
    assert_eq!(resp.status(), StatusCode::OK, "set permission should succeed");
}

/// Helper: publish a dependency-free package into a repository as admin.
async fn publish_package(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    repo: &str,
    package: &str,
    version: &str,
) {
    let pkg_json = format!(
        r#"{{"name":"{package}","version":"{version}","description":"authz test","main":"index.js"}}"#
    );
    let tarball = build_tarball(&pkg_json);
    let body = build_publish_body(package, version, "authz test", &tarball);

    let resp = client
        .put(format!("{}/{}/{}", base_url, repo, package))
        .bearer_auth(admin_token)
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "publish of {package} into {repo} should succeed"
    );
}

// ---------------------------------------------------------------------------
// Item 1 — vulns endpoints
// ---------------------------------------------------------------------------

/// An anonymous caller must not be able to confirm the existence of a package
/// hosted in a private repo through the vulns endpoint: the response is the
/// same 404 as for a package that does not exist at all. Admin (nominal) and
/// public-repo (anonymous) reads keep working.
#[tokio::test]
async fn test_vulns_private_repo_hidden_from_anonymous() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    publish_package(&client, &base_url, "test-token", "npm-secret", "@sec/hidden", "1.0.0").await;
    publish_package(&client, &base_url, "test-token", "npm-pub", "@pub/open", "1.0.0").await;

    // Anonymous probe of the private package -> 404 (was 200 before the fix).
    let resp = client
        .get(format!("{}/api/v1/vulns/@sec/hidden/1.0.0", base_url))
        .send()
        .await
        .expect("anonymous vulns request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "anonymous GET vulns of a private-repo package must 404"
    );

    // ... and it is indistinguishable from a package that does not exist.
    let resp = client
        .get(format!("{}/api/v1/vulns/@sec/ghost/1.0.0", base_url))
        .send()
        .await
        .expect("anonymous vulns request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a nonexistent package must return the same status"
    );

    // Nominal: admin still reads the private package's scan report.
    let resp = client
        .get(format!("{}/api/v1/vulns/@sec/hidden/1.0.0", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("admin vulns request failed");
    assert_eq!(resp.status(), StatusCode::OK, "admin GET vulns should succeed");
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["package"], "@sec/hidden");

    // Nominal: public-repo packages stay anonymously readable.
    let resp = client
        .get(format!("{}/api/v1/vulns/@pub/open/1.0.0", base_url))
        .send()
        .await
        .expect("anonymous vulns request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "anonymous GET vulns of a public-repo package must keep working"
    );
}

/// Rescan wipes stored scan results and triggers outbound OSV queries, so it
/// requires write access: a reader (read via role default, no write) gets 403;
/// a user whose read was explicitly revoked gets the non-revealing 404; admin
/// keeps working.
#[tokio::test]
async fn test_rescan_requires_write_permission() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    publish_package(&client, &base_url, "test-token", "npm-secret", "@sec/scanme", "1.0.0").await;

    // Reader: role default grants read on the repo, but not write -> 403.
    create_user(&client, &base_url, "test-token", "plain-reader", "reader").await;
    let reader_token =
        create_token_for_user(&client, &base_url, "test-token", "plain-reader", "t1").await;

    let resp = client
        .post(format!("{}/api/v1/vulns/@sec/scanme/1.0.0/rescan", base_url))
        .bearer_auth(&reader_token)
        .send()
        .await
        .expect("reader rescan request failed");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a reader without write permission must not trigger a rescan"
    );

    // User with an explicit no-read grant on the repo: the package must look
    // nonexistent -> 404 (not 403, which would confirm it exists).
    create_user(&client, &base_url, "test-token", "blind-user", "reader").await;
    let blind_token =
        create_token_for_user(&client, &base_url, "test-token", "blind-user", "t2").await;
    set_permission(
        &client,
        &base_url,
        "test-token",
        "blind-user",
        "npm-secret",
        json!({"can_read": false, "can_write": false, "can_delete": false, "can_admin": false}),
    )
    .await;

    let resp = client
        .post(format!("{}/api/v1/vulns/@sec/scanme/1.0.0/rescan", base_url))
        .bearer_auth(&blind_token)
        .send()
        .await
        .expect("blind rescan request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a caller without read access must see the same 404 as for a missing package"
    );

    // Nominal: admin rescan still works.
    let resp = client
        .post(format!("{}/api/v1/vulns/@sec/scanme/1.0.0/rescan", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("admin rescan request failed");
    assert_eq!(resp.status(), StatusCode::OK, "admin rescan should succeed");
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["package"], "@sec/scanme");
}

// ---------------------------------------------------------------------------
// Item 2 — GET /api/v1/repositories/{name}
// ---------------------------------------------------------------------------

/// Repository details (upstream URL, config) are only served to callers with
/// read access on the repo. A caller whose read was explicitly revoked gets
/// the same 404 as for a missing repository; the reader role default (read on
/// all repos) and admin access keep working; anonymous stays 401.
#[tokio::test]
async fn test_get_repository_requires_read_access() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // User with an explicit no-read grant on npm-secret -> non-revealing 404.
    create_user(&client, &base_url, "test-token", "no-read-user", "reader").await;
    let no_read_token =
        create_token_for_user(&client, &base_url, "test-token", "no-read-user", "t1").await;
    set_permission(
        &client,
        &base_url,
        "test-token",
        "no-read-user",
        "npm-secret",
        json!({"can_read": false, "can_write": false, "can_delete": false, "can_admin": false}),
    )
    .await;

    let resp = client
        .get(format!("{}/api/v1/repositories/npm-secret", base_url))
        .bearer_auth(&no_read_token)
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a caller without read access must see the same 404 as for a missing repo"
    );

    // Public repos stay visible to the same user (nominal non-admin path).
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-pub", base_url))
        .bearer_auth(&no_read_token)
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "public repo details must stay readable"
    );

    // A reader with no explicit grant reads the private repo through the role
    // default (`reader` = read on all repos, cf. check_repo_permission) —
    // asserting it documents the permission matrix this fix builds on.
    create_user(&client, &base_url, "test-token", "default-reader", "reader").await;
    let default_reader_token =
        create_token_for_user(&client, &base_url, "test-token", "default-reader", "t2").await;
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-secret", base_url))
        .bearer_auth(&default_reader_token)
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reader role default grants read on private repos"
    );

    // Nominal: admin sees the private repo's details.
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-secret", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("get repo request failed");
    assert_eq!(resp.status(), StatusCode::OK, "admin should read repo details");
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["name"], "npm-secret");
    assert_eq!(body["visibility"], "private");

    // Anonymous access to repository details stays denied outright.
    let resp = client
        .get(format!("{}/api/v1/repositories/npm-secret", base_url))
        .send()
        .await
        .expect("anonymous get repo request failed");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "repository details require authentication"
    );
}
