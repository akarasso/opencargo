use std::path::Path;
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, Visibility,
};
use axum::ServiceExt as _;
use tower::ServiceExt as _;
use opencargo::server;

/// Start a test server on a random port.
///
/// Returns `(base_url, port, server_handle, temp_dir)`.
async fn setup() -> (String, u16, tokio::task::JoinHandle<()>, TempDir) {
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
        repositories: vec![RepositoryConfig {
            name: "npm-private".to_string(),
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
    let port = addr.port();
    let base_url = format!("http://{}", addr);

    // Set the real base_url so tarball URLs are correct.
    config.server.base_url = base_url.clone();

    let state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let router = server::build_router(state);

    // Wrap with percent-encoded slash decoding before routing.
    let app = router
        .map_request(server::decode_percent_encoded_slashes)
        .into_make_service();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Wait for the server to be ready.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{}/health/live", &base_url)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, port, handle, tmp)
}

const PNPM: &str = "/home/alexandre/.local/share/pnpm/pnpm";

/// Helper: run a command, returning (exit_success, stdout, stderr).
async fn run_cmd(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env_home: &Path,
) -> (bool, String, String) {
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("HOME", env_home)
        .env("npm_config_store_dir", env_home.join(".pnpm-store"))
        // Prevent pnpm from reading any user-level config
        .env("XDG_CONFIG_HOME", env_home.join(".config"))
        .env("XDG_DATA_HOME", env_home.join(".local/share"))
        .env("XDG_CACHE_HOME", env_home.join(".cache"))
        .output()
        .await
        .expect("failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

// ---------------------------------------------------------------------------
// Test 1: Full roundtrip — publish then install and use a package
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_publish_and_install() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_and_install_inner(),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_and_install_inner() {
    let (_base_url, port, _handle, _server_tmp) = setup().await;

    let tmp = TempDir::new().expect("failed to create work temp dir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();

    // ---- Create the package to publish ----
    let pkg_dir = tmp.path().join("greeter-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();

    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({
            "name": "@test/greeter",
            "version": "1.0.0",
            "main": "index.js",
            "description": "Test E2E package"
        })
        .to_string(),
    )
    .unwrap();

    std::fs::write(
        pkg_dir.join("index.js"),
        r#"module.exports.greet = (name) => "Hello " + name;"#,
    )
    .unwrap();

    let npmrc = format!(
        "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
         //127.0.0.1:{port}/npm-private/:_authToken=test-token\n"
    );
    std::fs::write(pkg_dir.join(".npmrc"), &npmrc).unwrap();

    // ---- Publish ----
    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["publish", "--no-git-checks"],
        &pkg_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "pnpm publish failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Create consumer project ----
    let consumer_dir = tmp.path().join("consumer");
    std::fs::create_dir_all(&consumer_dir).unwrap();

    std::fs::write(
        consumer_dir.join("package.json"),
        serde_json::json!({
            "name": "consumer",
            "version": "1.0.0",
            "dependencies": {
                "@test/greeter": "1.0.0"
            }
        })
        .to_string(),
    )
    .unwrap();

    let consumer_npmrc = format!(
        "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
         //127.0.0.1:{port}/npm-private/:_authToken=test-token\n\
         node-linker=hoisted\n"
    );
    std::fs::write(consumer_dir.join(".npmrc"), &consumer_npmrc).unwrap();

    // ---- Install ----
    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["install", "--no-lockfile"],
        &consumer_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "pnpm install failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Verify the installed files exist ----
    let installed_index = consumer_dir.join("node_modules/@test/greeter/index.js");
    assert!(
        installed_index.exists(),
        "node_modules/@test/greeter/index.js should exist after install"
    );

    // ---- Run the module and check output ----
    let (ok, stdout, stderr) = run_cmd(
        "node",
        &["-e", "console.log(require('@test/greeter').greet('World'))"],
        &consumer_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "node execution failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        stdout.trim(),
        "Hello World",
        "unexpected output: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Publish multiple versions, install resolves latest matching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_publish_multiple_versions() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_multiple_versions_inner(),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_multiple_versions_inner() {
    let (_base_url, port, _handle, _server_tmp) = setup().await;

    let tmp = TempDir::new().expect("failed to create work temp dir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();

    let pkg_dir = tmp.path().join("versioned-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();

    let npmrc = format!(
        "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
         //127.0.0.1:{port}/npm-private/:_authToken=test-token\n"
    );
    std::fs::write(pkg_dir.join(".npmrc"), &npmrc).unwrap();

    // ---- Publish v1.0.0 ----
    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({
            "name": "@test/versioned",
            "version": "1.0.0",
            "main": "index.js",
            "description": "Versioned E2E package"
        })
        .to_string(),
    )
    .unwrap();

    std::fs::write(
        pkg_dir.join("index.js"),
        r#"module.exports.version = "1.0.0";"#,
    )
    .unwrap();

    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["publish", "--no-git-checks"],
        &pkg_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "pnpm publish v1.0.0 failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Publish v1.1.0 ----
    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({
            "name": "@test/versioned",
            "version": "1.1.0",
            "main": "index.js",
            "description": "Versioned E2E package"
        })
        .to_string(),
    )
    .unwrap();

    std::fs::write(
        pkg_dir.join("index.js"),
        r#"module.exports.version = "1.1.0";"#,
    )
    .unwrap();

    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["publish", "--no-git-checks"],
        &pkg_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "pnpm publish v1.1.0 failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Create consumer with ^1.0.0 ----
    let consumer_dir = tmp.path().join("consumer");
    std::fs::create_dir_all(&consumer_dir).unwrap();

    std::fs::write(
        consumer_dir.join("package.json"),
        serde_json::json!({
            "name": "consumer",
            "version": "1.0.0",
            "dependencies": {
                "@test/versioned": "^1.0.0"
            }
        })
        .to_string(),
    )
    .unwrap();

    let consumer_npmrc = format!(
        "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
         //127.0.0.1:{port}/npm-private/:_authToken=test-token\n\
         node-linker=hoisted\n"
    );
    std::fs::write(consumer_dir.join(".npmrc"), &consumer_npmrc).unwrap();

    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["install", "--no-lockfile"],
        &consumer_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "pnpm install failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Verify v1.1.0 was installed (latest matching ^1.0.0) ----
    let (ok, stdout, stderr) = run_cmd(
        "node",
        &["-e", "console.log(require('@test/versioned').version)"],
        &consumer_dir,
        &fake_home,
    )
    .await;
    assert!(
        ok,
        "node execution failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        stdout.trim(),
        "1.1.0",
        "should have installed v1.1.0 (latest matching ^1.0.0), got: {}",
        stdout.trim()
    );
}

// ---------------------------------------------------------------------------
// Test 3: Publish without auth token should fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_publish_without_auth_fails() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_without_auth_fails_inner(),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_without_auth_fails_inner() {
    let (_base_url, port, _handle, _server_tmp) = setup().await;

    let tmp = TempDir::new().expect("failed to create work temp dir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();

    let pkg_dir = tmp.path().join("noauth-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();

    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({
            "name": "@test/noauth",
            "version": "1.0.0",
            "main": "index.js",
            "description": "Package published without auth"
        })
        .to_string(),
    )
    .unwrap();

    std::fs::write(pkg_dir.join("index.js"), "module.exports = {};").unwrap();

    // .npmrc with registry but NO auth token
    let npmrc = format!(
        "@test:registry=http://127.0.0.1:{port}/npm-private/\n"
    );
    std::fs::write(pkg_dir.join(".npmrc"), &npmrc).unwrap();

    // ---- Attempt publish without auth ----
    let (ok, stdout, stderr) = run_cmd(
        PNPM,
        &["publish", "--no-git-checks"],
        &pkg_dir,
        &fake_home,
    )
    .await;

    assert!(
        !ok,
        "pnpm publish without auth should have failed but succeeded.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
