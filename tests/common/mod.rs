#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
//! SQLite-only by design: these assertions guarantee the schema, not the
//! ports (designs-next/ports-and-adapters.md 7.4).

#![allow(dead_code)]

pub mod contract;
pub mod fake_idp;
pub mod fake_osv;
pub mod fake_upstream;
pub mod fakes;
pub mod manifests;
pub mod pypi;
pub mod faults;
pub mod nuget;
pub mod upstream_tap;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

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
use opencargo::policy::rules::PolicyConfig;
use opencargo::policy::{PolicyEngine, Tuning};
use opencargo::proxy::UpstreamAuth;
use opencargo::server;
use opencargo::server::shutdown::{ServerHandle, Shutdown};

/// The one static token every spawned server accepts.
pub const STATIC_TOKEN: &str = "test-token";

pub struct SpawnOpts {
    pub anonymous_read: bool,
    pub repositories: Vec<RepositoryConfig>,
    pub proxy: ProxyConfig,
    pub vuln: VulnScanConfig,
    pub policy: HashMap<String, PolicyConfig>,
    /// Replaces the policy engine's timing knobs after `build_state`.
    pub policy_tuning: Option<Tuning>,
    /// Puts the storage and the permission store behind switches.
    pub outage: Option<faults::Outage>,
    pub sso: opencargo::config::SsoConfig,
    /// The URL clients reach the server by, when a reverse proxy fronts it.
    pub public_url: Option<String>,
    /// Take the writer lease, on terms short enough for a test to outwait.
    pub lease: bool,
    /// `[auth].static_tokens`.
    pub static_tokens: Vec<String>,
    /// `[server].endpoint_drain`.
    pub endpoint_drain: String,
}

impl Default for SpawnOpts {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            repositories: Vec::new(),
            proxy: ProxyConfig::default(),
            vuln: VulnScanConfig::default(),
            policy: HashMap::new(),
            policy_tuning: None,
            outage: None,
            sso: Default::default(),
            public_url: None,
            lease: false,
            static_tokens: vec![STATIC_TOKEN.to_string()],
            endpoint_drain: "0s".to_string(),
        }
    }
}

pub struct TestServer {
    pub base_url: String,
    pub port: u16,
    pub handle: tokio::task::JoinHandle<()>,
    pub tmp: TempDir,
    /// The store the server ran on, kept so a restart finds its bytes.
    pub storage: opencargo::config::StorageConfig,
    pub lease: Option<opencargo::app::lease::LeaseGuard>,
    pub srv: ServerHandle,
    pub shutdown: Shutdown,
    endpoint_drain: Duration,
    grace: Duration,
}

impl TestServer {
    /// Stop serving and give the lease back, as a clean shutdown would.
    pub async fn stop(&mut self) {
        self.handle.abort();
        if let Some(lease) = self.lease.take() {
            lease.release().await;
        }
    }

    /// Production's drain, then the end of serving, then the lease: what
    /// `main` does on SIGTERM, without a signal.
    pub async fn drain(&mut self) {
        self.shutdown.drain(&self.srv, self.endpoint_drain, self.grace).await;
        (&mut self.handle).await.ok();
        if let Some(lease) = self.lease.take() {
            lease.release().await;
        }
    }
}

/// Lease terms a test can outwait: renewed every second, stale after three.
pub fn short_lease(server: &mut ServerConfig) {
    server.lease_wait = "4s".to_string();
    server.lease_stale_after = "3s".to_string();
    server.lease_renew = "1s".to_string();
}

/// The config every spawned server runs with: storage and database under `tmp`.
fn test_config(tmp: &TempDir, base_url: &str, opts: SpawnOpts) -> Config {
    let public_url = opts.public_url.clone().unwrap_or_else(|| base_url.to_string());
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("opencargo.db");
    let mut config = Config {
        server: ServerConfig {
            bind: base_url.trim_start_matches("http://").to_string(),
            base_url: public_url,
            storage_path: storage_path
                .to_str()
                .expect("non-utf8 temp path")
                .to_string(),
            lease: opts.lease,
            endpoint_drain: opts.endpoint_drain.clone(),
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
            static_tokens: opts.static_tokens.clone(),
            sso: opts.sso.clone(),
            ..Default::default()
        },
        proxy: opts.proxy,
        repositories: opts.repositories,
        vuln_scan: opts.vuln,
        policy: opts.policy,
        ..Default::default()
    };
    short_lease(&mut config.server);
    config
}

/// Whether this run puts every test server on S3 (`OPENCARGO_TEST_STORAGE=s3`,
/// the endpoint and credentials in the adapter's own variables).
pub fn storage_is_s3() -> bool {
    std::env::var("OPENCARGO_TEST_STORAGE").as_deref() == Ok("s3")
}

/// The storage switch every test config goes through: under S3, one bucket
/// and a prefix of its own per server.
pub fn switch_storage(config: &mut Config) {
    if !storage_is_s3() {
        return;
    }
    config.storage.backend = opencargo::config::StorageKind::S3;
    config.storage.s3.bucket =
        std::env::var("OPENCARGO_TEST_S3_BUCKET").unwrap_or_else(|_| "opencargo-test".to_string());
    config.storage.s3.allow_http = true;
    config.storage.s3.part_size_mib = 5;
    if config.storage.s3.prefix.is_empty() {
        config.storage.s3.prefix = format!("t/{}", uuid::Uuid::new_v4().simple());
    }
}

/// `server::build_state` behind the switch, refusing a vacuous S3 run: a
/// switched server that did not build an S3 store fails the test.
pub async fn start(config: &mut Config) -> anyhow::Result<server::Started> {
    start_with(config, ServerHandle::new(), Shutdown::new()).await
}

pub async fn start_with(
    config: &mut Config,
    srv: ServerHandle,
    shutdown: Shutdown,
) -> anyhow::Result<server::Started> {
    switch_storage(config);
    let started = server::build_state(config, srv, shutdown).await?;
    let want = if storage_is_s3() { "s3" } else { "fs" };
    assert_eq!(started.state.storage_backend, want, "the storage switch was not applied");
    Ok(started)
}

/// The state alone, for the files that build their own config; a lease it
/// took stops renewing when its guard drops here.
pub async fn build_state(config: &mut Config) -> anyhow::Result<opencargo::server::AppState> {
    Ok(start(config).await?.state)
}

/// Start an opencargo on a random loopback port, ready to serve.
pub async fn spawn_server(opts: SpawnOpts) -> TestServer {
    let tmp = TempDir::new().expect("failed to create temp dir");
    spawn_in(tmp, opts, None).await
}

/// Stop `server` and start another on its database and storage: a restart.
pub async fn respawn(mut server: TestServer, opts: SpawnOpts) -> TestServer {
    server.stop().await;
    spawn_in(server.tmp, opts, Some(server.storage)).await
}

/// The store `server` runs on, built again through the composition root.
pub async fn storage_of(server: &TestServer) -> std::sync::Arc<dyn opencargo::storage::StorageBackend> {
    let mut config = test_config(&server.tmp, "http://127.0.0.1:0", SpawnOpts::default());
    config.storage = server.storage.clone();
    let stores = server::open_stores(&server.tmp.path().join("opencargo.db"))
        .await
        .expect("failed to open the server database");
    server::storage_for(&config, stores.multipart()).expect("failed to build the server's store")
}

/// The config `server` runs with, its store included.
pub fn config_of(server: &TestServer) -> Config {
    let mut config = test_config(&server.tmp, &server.base_url, SpawnOpts::default());
    config.storage = server.storage.clone();
    config
}

/// Every key the server's store holds, sorted.
pub async fn stored_keys(server: &TestServer) -> Vec<String> {
    use futures_util::TryStreamExt;
    let mut keys: Vec<String> = storage_of(server)
        .await
        .list("")
        .map_ok(|meta| meta.key)
        .try_collect()
        .await
        .expect("failed to list the server's store");
    keys.sort();
    keys
}

async fn spawn_in(
    tmp: TempDir,
    opts: SpawnOpts,
    storage: Option<opencargo::config::StorageConfig>,
) -> TestServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind to random port");
    listener.set_nonblocking(true).expect("a non-blocking listener");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{addr}");

    let tuning = opts.policy_tuning;
    let outage = opts.outage.clone();
    let mut config = test_config(&tmp, &base_url, opts);
    if let Some(storage) = storage {
        config.storage = storage;
    }
    let srv = ServerHandle::new();
    let shutdown = Shutdown::new();
    let server::Started { mut state, lease } = start_with(&mut config, srv.clone(), shutdown.clone())
        .await
        .expect("failed to build app state");
    if let Some(tuning) = tuning {
        state.policy = PolicyEngine::new_tuned(
            state.policy_store.clone(),
            &config.policy,
            state.vuln_scanner.clone(),
            state.events.clone(),
            state.proxy.clone(),
            tuning,
        );
    }
    if let Some(outage) = outage {
        outage.install(&mut state);
    }
    let app = server::build_router(state)
        .map_request(server::decode_percent_encoded_slashes)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

    let serving = axum_server::from_tcp(listener)
        .expect("a listener axum_server accepts")
        .handle(srv.clone());
    let handle = tokio::spawn(async move {
        serving.serve(app).await.ok();
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
        storage: config.storage.clone(),
        lease,
        srv,
        shutdown,
        endpoint_drain: config.server.endpoint_drain().expect("a valid endpoint_drain"),
        grace: config.server.shutdown_grace().expect("a valid shutdown_grace"),
    }
}

/// The error a restart of `server` on its own database refuses to start
/// with under `opts`; the running server is left alone.
pub async fn start_error_in(server: &TestServer, opts: SpawnOpts) -> String {
    let mut config = test_config(&server.tmp, "http://127.0.0.1:0", opts);
    start(&mut config)
        .await
        .err()
        .expect("the start should be refused")
        .to_string()
}

/// The error a server refuses to start with under this repository seed.
pub async fn seed_error(repositories: Vec<RepositoryConfig>) -> String {
    seed_error_opts(SpawnOpts {
        repositories,
        ..Default::default()
    })
    .await
}

/// The error a server refuses to start with under these options.
pub async fn seed_error_opts(opts: SpawnOpts) -> String {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let mut config = test_config(&tmp, "http://127.0.0.1:0", opts);
    build_state(&mut config)
        .await
        .err()
        .expect("the seed should be refused")
        .to_string()
}

type Db = sqlx::SqlitePool;

async fn open_db(server: &TestServer) -> Db {
    open_db_at(&server.tmp.path().join("opencargo.db")).await
}

async fn open_db_at(db_path: &Path) -> Db {
    Db::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database")
}

/// Every table of the SQLite file at `db_path`, sorted: what proves a
/// refused start wrote nothing.
pub async fn table_names(db_path: &Path) -> Vec<String> {
    let pool = open_db_at(db_path).await;
    let names = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(&pool)
        .await
        .expect("failed to list tables");
    pool.close().await;
    names
}

/// One `policy_resolutions` row as the report will read it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PolicyRow {
    pub id: i64,
    pub requested_repo: String,
    pub member_repo: String,
    pub format: String,
    pub name: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub published_at: Option<String>,
    pub date_source: String,
    pub actor: String,
    pub actor_kind: String,
    pub user_id: Option<i64>,
    pub would_block: bool,
    pub unknown: bool,
}

impl PolicyRow {
    pub fn published(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let at = self.published_at.as_deref()?;
        Some(
            chrono::DateTime::parse_from_rfc3339(at)
                .expect("published_at is RFC 3339")
                .with_timezone(&chrono::Utc),
        )
    }
}

/// Every recorded resolution, oldest first.
pub async fn policy_rows(server: &TestServer) -> Vec<PolicyRow> {
    let pool = open_db(server).await;
    let rows = sqlx::query_as::<_, PolicyRow>("SELECT * FROM policy_resolutions ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("failed to read policy rows");
    pool.close().await;
    rows
}

/// One `policy_verdicts` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VerdictRow {
    pub resolution_id: i64,
    pub rule: String,
    pub verdict: String,
    pub reason: String,
}

/// Every verdict, by resolution then rule.
pub async fn policy_verdicts(server: &TestServer) -> Vec<VerdictRow> {
    let pool = open_db(server).await;
    let rows = sqlx::query_as::<_, VerdictRow>(
        "SELECT resolution_id, rule, verdict, reason FROM policy_verdicts ORDER BY resolution_id, rule",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read policy verdicts");
    pool.close().await;
    rows
}

/// `(verdict, reason)` of `rule` on resolution `id`; a rule off in config
/// left no row and that is a failure here.
pub fn verdict_of<'a>(verdicts: &'a [VerdictRow], id: i64, rule: &str) -> (&'a str, &'a str) {
    verdicts
        .iter()
        .find(|v| v.resolution_id == id && v.rule == rule)
        .map(|v| (v.verdict.as_str(), v.reason.as_str()))
        .unwrap_or_else(|| panic!("no {rule} verdict on resolution {id}: {verdicts:#?}"))
}

/// The rules that left a verdict on resolution `id`.
pub fn rules_of(verdicts: &[VerdictRow], id: i64) -> Vec<&str> {
    verdicts
        .iter()
        .filter(|v| v.resolution_id == id)
        .map(|v| v.rule.as_str())
        .collect()
}

/// Rows are written after the response: poll until `n` exist, then insist
/// on exactly `n`.
pub async fn wait_for_policy_rows(server: &TestServer, n: usize) -> Vec<PolicyRow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = policy_rows(server).await;
        if rows.len() >= n || std::time::Instant::now() >= deadline {
            assert_eq!(rows.len(), n, "policy rows: {rows:#?}");
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Absence is never proven with a sleep: pull one artifact that records
/// (the sentinel) and wait for the row count it must bring the table to.
/// The channel is FIFO, so the sentinel's row proves every earlier event
/// was examined.
pub async fn sentinel(server: &TestServer, url: &str, rows_after: usize) -> Vec<PolicyRow> {
    let resp = reqwest::get(url).await.expect("sentinel request failed");
    assert_eq!(resp.status(), StatusCode::OK, "sentinel GET {url}");
    wait_for_policy_rows(server, rows_after).await
}

/// `users.id` of `username`.
pub async fn user_id(server: &TestServer, username: &str) -> i64 {
    let pool = open_db(server).await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
        .bind(username)
        .fetch_one(&pool)
        .await
        .expect("user exists");
    pool.close().await;
    id
}

/// `GET /api/v1/policy/report?{query}` as the static admin token.
pub async fn report(server: &TestServer, query: &str) -> Value {
    let resp = reqwest::Client::new()
        .get(format!("{}/api/v1/policy/report?{query}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("report request failed");
    assert_eq!(resp.status(), StatusCode::OK, "report?{query}");
    resp.json().await.expect("report is json")
}

/// A user created through the admin API plus one API token named `name`.
pub async fn named_token(
    client: &reqwest::Client,
    base_url: &str,
    username: &str,
    name: &str,
) -> String {
    create_user(client, base_url, STATIC_TOKEN, username, "reader").await;
    add_token(client, base_url, username, name).await
}

/// One more API token named `name` for an existing user.
pub async fn add_token(
    client: &reqwest::Client,
    base_url: &str,
    username: &str,
    name: &str,
) -> String {
    let resp = client
        .post(format!("{base_url}/api/v1/users/{username}/tokens"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "name": name }))
        .send()
        .await
        .expect("create token request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = resp.json().await.expect("invalid json");
    body["token"].as_str().expect("token").to_string()
}

/// Move a hosted version's `published_at` `hours` into the past.
pub async fn backdate_version(server: &TestServer, package: &str, version: &str, hours: i64) {
    let pool = open_db(server).await;
    sqlx::query(
        "UPDATE versions SET published_at = datetime('now', ?3 || ' hours')          WHERE version = ?2 AND package_id IN (SELECT id FROM packages WHERE name = ?1)",
    )
    .bind(package)
    .bind(version)
    .bind(format!("-{hours}"))
    .execute(&pool)
    .await
    .expect("failed to backdate the version");
    pool.close().await;
}

#[derive(Default)]
pub struct ProxyOpts {
    pub dl_allow_private: bool,
    pub upstream_auth: Option<UpstreamAuth>,
    pub token_realms: Vec<String>,
    pub file_hosts: Vec<String>,
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
        file_hosts: opts.file_hosts,
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
        // Pinned mtime: the default is "now" at DOS 2 s granularity, which made
        // byte-exact comparisons of two builds flaky across a boundary.
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());

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
