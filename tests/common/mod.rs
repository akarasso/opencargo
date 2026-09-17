#![allow(dead_code)]

pub mod fake_osv;
pub mod fake_upstream;
pub mod upstream_tap;

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

use axum::ServiceExt as _;
use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::Digest;
use tempfile::TempDir;
use tower::ServiceExt as _;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, ProxyConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility, VulnScanConfig,
};
use opencargo::proxy::UpstreamAuth;
use opencargo::server;

/// The one static token every spawned server accepts.
pub const STATIC_TOKEN: &str = "test-token";

pub struct SpawnOpts {
    pub anonymous_read: bool,
    pub repositories: Vec<RepositoryConfig>,
    pub proxy: ProxyConfig,
    pub vuln: VulnScanConfig,
}

impl Default for SpawnOpts {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            repositories: Vec::new(),
            proxy: ProxyConfig::default(),
            vuln: VulnScanConfig::default(),
        }
    }
}

pub struct TestServer {
    pub base_url: String,
    pub port: u16,
    pub handle: tokio::task::JoinHandle<()>,
    pub tmp: TempDir,
}

/// The config every spawned server runs with: storage and database under `tmp`.
fn test_config(tmp: &TempDir, base_url: &str, opts: SpawnOpts) -> Config {
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("opencargo.db");
    Config {
        server: ServerConfig {
            bind: base_url.trim_start_matches("http://").to_string(),
            base_url: base_url.to_string(),
            storage_path: storage_path
                .to_str()
                .expect("non-utf8 temp path")
                .to_string(),
            ..Default::default()
        },
        database: DatabaseConfig {
            url: format!(
                "sqlite:{}?mode=rwc",
                db_path.to_str().expect("non-utf8 temp path")
            ),
        },
        auth: AuthConfig {
            anonymous_read: opts.anonymous_read,
            static_tokens: vec![STATIC_TOKEN.to_string()],
            ..Default::default()
        },
        proxy: opts.proxy,
        repositories: opts.repositories,
        vuln_scan: opts.vuln,
        ..Default::default()
    }
}

/// Start an opencargo on a random loopback port, ready to serve.
pub async fn spawn_server(opts: SpawnOpts) -> TestServer {
    let tmp = TempDir::new().expect("failed to create temp dir");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind to random port");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{addr}");

    let config = test_config(&tmp, &base_url, opts);
    let state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let app = server::build_router(state)
        .map_request(server::decode_percent_encoded_slashes)
        .into_make_service();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{base_url}/health/live")).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    TestServer {
        base_url,
        port: addr.port(),
        handle,
        tmp,
    }
}

/// The error a server refuses to start with under this repository seed.
pub async fn seed_error(repositories: Vec<RepositoryConfig>) -> String {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let opts = SpawnOpts {
        repositories,
        ..Default::default()
    };
    let config = test_config(&tmp, "http://127.0.0.1:0", opts);
    server::build_state(&config)
        .await
        .err()
        .expect("the seed should be refused")
        .to_string()
}

#[derive(Default)]
pub struct ProxyOpts {
    pub dl_allow_private: bool,
    pub upstream_auth: Option<UpstreamAuth>,
    pub token_realms: Vec<String>,
}

pub fn hosted(name: &str, fmt: RepositoryFormat, vis: Visibility) -> RepositoryConfig {
    RepositoryConfig {
        name: name.to_string(),
        repo_type: RepositoryType::Hosted,
        format: fmt,
        visibility: vis,
        ..Default::default()
    }
}

pub fn proxy(name: &str, fmt: RepositoryFormat, upstream: &str) -> RepositoryConfig {
    proxy_with(name, fmt, upstream, ProxyOpts::default())
}

pub fn proxy_with(
    name: &str,
    fmt: RepositoryFormat,
    upstream: &str,
    opts: ProxyOpts,
) -> RepositoryConfig {
    RepositoryConfig {
        name: name.to_string(),
        repo_type: RepositoryType::Proxy,
        format: fmt,
        visibility: Visibility::Public,
        upstream: Some(upstream.to_string()),
        upstream_auth: opts.upstream_auth,
        token_realms: opts.token_realms,
        dl_allow_private: opts.dl_allow_private,
        ..Default::default()
    }
}

pub fn group(name: &str, fmt: RepositoryFormat, members: &[&str]) -> RepositoryConfig {
    RepositoryConfig {
        name: name.to_string(),
        repo_type: RepositoryType::Group,
        format: fmt,
        visibility: Visibility::Public,
        members: Some(members.iter().map(|m| m.to_string()).collect()),
        ..Default::default()
    }
}

/// Push every TTL-bound cache row into the past; immutable rows are untouched.
pub async fn expire_entries(server: &TestServer) {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    sqlx::query(
        "UPDATE proxy_cache_entries SET expires_at = datetime('now', '-1 second') \
         WHERE expires_at IS NOT NULL",
    )
    .execute(&pool)
    .await
    .expect("failed to expire cache entries");
    pool.close().await;
}

/// Build a gzip'd tar archive in memory containing `package/package.json`.
pub fn build_tarball(package_json_content: &str) -> Vec<u8> {
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
pub fn build_npm_publish_body(
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

/// Build the binary body for Cargo publish requests: LE u32 metadata length,
/// metadata JSON, LE u32 crate length, crate bytes.
pub fn build_cargo_publish_body(metadata_json: &str, crate_data: &[u8]) -> Vec<u8> {
    let json_bytes = metadata_json.as_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    body.extend_from_slice(json_bytes);
    body.extend_from_slice(&(crate_data.len() as u32).to_le_bytes());
    body.extend_from_slice(crate_data);
    body
}

/// Build a minimal .crate file (gzip compressed data).
pub fn build_crate_data() -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"fake crate content").unwrap();
    encoder.finish().unwrap()
}

/// Build a Go module zip archive in memory: a go.mod and one .go file.
pub fn build_go_module_zip(module_name: &str, version: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let go_mod_path = format!("{}@{}/go.mod", module_name, version);
        zip_writer.start_file(&go_mod_path, options).unwrap();
        let go_mod_content = format!("module {}\n\ngo 1.21\n", module_name);
        zip_writer.write_all(go_mod_content.as_bytes()).unwrap();

        let go_file_path = format!("{}@{}/main.go", module_name, version);
        zip_writer.start_file(&go_file_path, options).unwrap();
        let go_content = format!(
            "package {}\n\nfunc Hello() string {{ return \"hello\" }}\n",
            module_name.split('/').next_back().unwrap_or("main")
        );
        zip_writer.write_all(go_content.as_bytes()).unwrap();

        zip_writer.finish().unwrap();
    }
    buf
}

/// Publish a Go module into `repo` with the static token.
pub async fn publish_go_module(
    client: &reqwest::Client,
    base_url: &str,
    repo: &str,
    module_name: &str,
    version: &str,
) {
    let zip_data = build_go_module_zip(module_name, version);
    let resp = client
        .put(format!("{base_url}/{repo}/{module_name}/@v/{version}"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", "application/zip")
        .body(zip_data)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "go publish failed: {:?}",
        resp.text().await
    );
}

/// Compute sha256 digest in the OCI format "sha256:hex..."
pub fn sha256_digest(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Upload a blob (monolithic PUT) into `image` (`{repo}/{name}`) and return its digest.
pub async fn push_blob(
    client: &reqwest::Client,
    base_url: &str,
    image: &str,
    blob_data: &[u8],
) -> String {
    let digest = sha256_digest(blob_data);

    let resp = client
        .post(format!("{base_url}/v2/{image}/blobs/uploads/"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("start upload request failed");
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "start upload failed");

    let location = resp
        .headers()
        .get("location")
        .expect("missing Location header")
        .to_str()
        .expect("invalid location header")
        .to_string();

    let resp = client
        .put(format!("{base_url}{location}?digest={digest}"))
        .bearer_auth(STATIC_TOKEN)
        .body(blob_data.to_vec())
        .send()
        .await
        .expect("complete upload request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "complete upload failed: {:?}",
        resp.text().await
    );

    digest
}

/// Encode username:password as a Basic auth header value.
pub fn basic_auth_header(username: &str, password: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

/// Create a user via the admin API and return the response JSON.
pub async fn create_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    role: &str,
) -> Value {
    let resp = client
        .post(format!("{base_url}/api/v1/users"))
        .bearer_auth(admin_token)
        .json(&json!({ "username": username, "role": role }))
        .send()
        .await
        .expect("create user request failed");

    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from create user");
    assert_eq!(status, StatusCode::CREATED, "create user failed: {body:?}");
    body
}

/// Run a command, returning (exit_success, stdout, stderr).
pub async fn run_cmd(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &OsStr)],
) -> (bool, String, String) {
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .output()
        .await
        .expect("failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

/// The client binary named by `var` (`PNPM_BIN`, `CARGO_BIN`, ...), defaulting
/// to the lowercase name on PATH. An absent binary prints `skipped:` and yields
/// `None`, unless `OPENCARGO_E2E_REQUIRE=1` makes the absence a failure.
pub fn client_bin(var: &str) -> Option<String> {
    let default = var.trim_end_matches("_BIN").to_ascii_lowercase();
    let bin = std::env::var(var).unwrap_or(default);
    if std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .is_ok()
    {
        return Some(bin);
    }
    assert!(
        std::env::var("OPENCARGO_E2E_REQUIRE").as_deref() != Ok("1"),
        "{bin} is required by OPENCARGO_E2E_REQUIRE=1 but was not found (set {var})"
    );
    println!("skipped: {bin} not found (install it or set {var})");
    None
}
