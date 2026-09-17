#![allow(dead_code)]

pub mod upstream_tap;

use axum::ServiceExt as _;
use base64::Engine;
use serde_json::{json, Value};
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

/// Start an opencargo on a random loopback port, ready to serve.
pub async fn spawn_server(opts: SpawnOpts) -> TestServer {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("opencargo.db");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind to random port");
    let addr = listener.local_addr().expect("no local addr");
    let base_url = format!("http://{addr}");

    let config = Config {
        server: ServerConfig {
            bind: addr.to_string(),
            base_url: base_url.clone(),
            storage_path: storage_path.to_str().expect("non-utf8 temp path").to_string(),
            ..Default::default()
        },
        database: DatabaseConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.to_str().expect("non-utf8 temp path")),
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
    };

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
