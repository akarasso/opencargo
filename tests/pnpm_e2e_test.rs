mod common;

use std::ffi::OsStr;
use std::path::Path;
use tempfile::TempDir;

use common::{client_bin, group, hosted, run_cmd, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};

/// Start a test server on a random port with a public `npm-private` repository.
///
/// Returns `(base_url, port, server_handle, temp_dir)`.
async fn setup() -> (String, u16, tokio::task::JoinHandle<()>, TempDir) {
    setup_with(
        true,
        vec![hosted("npm-private", RepositoryFormat::Npm, Visibility::Public)],
    )
    .await
}

async fn setup_with(
    anonymous_read: bool,
    repositories: Vec<RepositoryConfig>,
) -> (String, u16, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        anonymous_read,
        repositories,
        ..Default::default()
    })
    .await;
    (server.base_url, server.port, server.handle, server.tmp)
}

/// Run a command under an isolated `HOME` so pnpm reads no user-level config.
async fn run_in_home(
    program: &str,
    args: &[&str],
    cwd: &Path,
    home: &Path,
) -> (bool, String, String) {
    let store = home.join(".pnpm-store");
    let config = home.join(".config");
    let data = home.join(".local/share");
    let cache = home.join(".cache");
    let env: [(&str, &OsStr); 5] = [
        ("HOME", home.as_os_str()),
        ("npm_config_store_dir", store.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("XDG_DATA_HOME", data.as_os_str()),
        ("XDG_CACHE_HOME", cache.as_os_str()),
    ];
    run_cmd(program, args, cwd, &env).await
}

// ---------------------------------------------------------------------------
// Test 1: Full roundtrip — publish then install and use a package
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_publish_and_install() {
    let Some(pnpm) = client_bin("PNPM_BIN") else {
        return;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_and_install_inner(&pnpm),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_and_install_inner(pnpm: &str) {
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
    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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
    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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
    let (ok, stdout, stderr) = run_in_home(
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
// Install through a group with anonymous reads disabled and the token declared
// on the group path only: tarball URLs must point at the group, not the member.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_install_through_private_group() {
    let Some(pnpm) = client_bin("PNPM_BIN") else {
        return;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_install_through_private_group_inner(&pnpm),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_install_through_private_group_inner(pnpm: &str) {
    let (base_url, port, _handle, _server_tmp) = setup_with(
        false,
        vec![
            hosted("npm-private", RepositoryFormat::Npm, Visibility::Private),
            RepositoryConfig {
                visibility: Visibility::Private,
                ..group("npm-all", RepositoryFormat::Npm, &["npm-private"])
            },
        ],
    )
    .await;

    let tmp = TempDir::new().expect("failed to create work temp dir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();

    let pkg_dir = tmp.path().join("grouped-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({"name": "@test/grouped", "version": "1.0.0", "main": "index.js"}).to_string(),
    )
    .unwrap();
    std::fs::write(pkg_dir.join("index.js"), "module.exports = 'grouped';").unwrap();
    std::fs::write(
        pkg_dir.join(".npmrc"),
        format!(
            "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
             //127.0.0.1:{port}/npm-private/:_authToken=test-token\n"
        ),
    )
    .unwrap();

    let (ok, stdout, stderr) =
        run_in_home(pnpm, &["publish", "--no-git-checks"], &pkg_dir, &fake_home).await;
    assert!(ok, "pnpm publish failed.\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let client = reqwest::Client::new();
    let meta: serde_json::Value = client
        .get(format!("{base_url}/npm-all/@test/grouped"))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tarball = meta["versions"]["1.0.0"]["dist"]["tarball"].as_str().unwrap();
    assert!(
        tarball.contains("/npm-all/@test/grouped/-/"),
        "tarball must be served through the requested group, got {tarball}"
    );

    let consumer_dir = tmp.path().join("consumer");
    std::fs::create_dir_all(&consumer_dir).unwrap();
    std::fs::write(
        consumer_dir.join("package.json"),
        serde_json::json!({"name": "consumer", "version": "1.0.0", "dependencies": {"@test/grouped": "1.0.0"}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        consumer_dir.join(".npmrc"),
        format!(
            "@test:registry=http://127.0.0.1:{port}/npm-all/\n\
             //127.0.0.1:{port}/npm-all/:_authToken=test-token\n\
             node-linker=hoisted\n"
        ),
    )
    .unwrap();

    let (ok, stdout, stderr) =
        run_in_home(pnpm, &["install", "--no-lockfile"], &consumer_dir, &fake_home).await;
    assert!(ok, "pnpm install through group failed.\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(consumer_dir.join("node_modules/@test/grouped/index.js").exists());
}

// ---------------------------------------------------------------------------
// A public group must not expose a private member to callers who cannot read it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_public_group_hides_private_member_from_anonymous() {
    let Some(pnpm) = client_bin("PNPM_BIN") else {
        return;
    };
    let pnpm = pnpm.as_str();
    let (base_url, port, _handle, _server_tmp) = setup_with(
        true,
        vec![
            hosted("npm-private", RepositoryFormat::Npm, Visibility::Private),
            group("npm-all", RepositoryFormat::Npm, &["npm-private"]),
        ],
    )
    .await;

    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    let pkg_dir = tmp.path().join("secret-pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        serde_json::json!({"name": "@test/secret", "version": "1.0.0", "main": "index.js"}).to_string(),
    )
    .unwrap();
    std::fs::write(pkg_dir.join("index.js"), "module.exports = 'secret';").unwrap();
    std::fs::write(
        pkg_dir.join(".npmrc"),
        format!(
            "@test:registry=http://127.0.0.1:{port}/npm-private/\n\
             //127.0.0.1:{port}/npm-private/:_authToken=test-token\n"
        ),
    )
    .unwrap();
    let (ok, stdout, stderr) =
        run_in_home(pnpm, &["publish", "--no-git-checks"], &pkg_dir, &fake_home).await;
    assert!(ok, "pnpm publish failed.\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let client = reqwest::Client::new();
    let meta_url = format!("{base_url}/npm-all/@test/secret");
    let tarball_url = format!("{base_url}/npm-all/@test/secret/-/secret-1.0.0.tgz");

    let anon_meta = client.get(&meta_url).send().await.unwrap().status();
    assert_eq!(anon_meta.as_u16(), 404, "anonymous metadata through public group must not resolve a private member");
    let anon_tarball = client.get(&tarball_url).send().await.unwrap().status();
    assert_eq!(anon_tarball.as_u16(), 404, "anonymous tarball through public group must not resolve a private member");

    let auth_meta = client.get(&meta_url).bearer_auth("test-token").send().await.unwrap().status();
    assert_eq!(auth_meta.as_u16(), 200);
    let auth_tarball = client.get(&tarball_url).bearer_auth("test-token").send().await.unwrap().status();
    assert_eq!(auth_tarball.as_u16(), 200);
}

// ---------------------------------------------------------------------------
// Test 2: Publish multiple versions, install resolves latest matching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pnpm_publish_multiple_versions() {
    let Some(pnpm) = client_bin("PNPM_BIN") else {
        return;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_multiple_versions_inner(&pnpm),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_multiple_versions_inner(pnpm: &str) {
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

    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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

    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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

    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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
    let (ok, stdout, stderr) = run_in_home(
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
    let Some(pnpm) = client_bin("PNPM_BIN") else {
        return;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        pnpm_publish_without_auth_fails_inner(&pnpm),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 120s");
}

async fn pnpm_publish_without_auth_fails_inner(pnpm: &str) {
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
    let (ok, stdout, stderr) = run_in_home(
        pnpm,
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
