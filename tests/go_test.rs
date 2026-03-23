use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, Visibility,
};
use opencargo::server;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a Go module zip archive in memory.
/// Contains a go.mod file and a dummy .go file.
fn build_go_module_zip(module_name: &str, version: &str) -> Vec<u8> {
    use std::io::Write;

    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // Add go.mod
        let go_mod_path = format!("{}@{}/go.mod", module_name, version);
        zip_writer.start_file(&go_mod_path, options).unwrap();
        let go_mod_content = format!("module {}\n\ngo 1.21\n", module_name);
        zip_writer.write_all(go_mod_content.as_bytes()).unwrap();

        // Add a dummy .go file
        let go_file_path = format!("{}@{}/main.go", module_name, version);
        zip_writer.start_file(&go_file_path, options).unwrap();
        let go_content = format!(
            "package {}\n\nfunc Hello() string {{ return \"hello\" }}\n",
            module_name.split('/').last().unwrap_or("main")
        );
        zip_writer.write_all(go_content.as_bytes()).unwrap();

        zip_writer.finish().unwrap();
    }
    buf
}

/// Start a test server on a random port with a Go hosted repository.
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
            name: "go-hosted".to_string(),
            repo_type: RepositoryType::Hosted,
            format: RepositoryFormat::Go,
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

/// Publish a Go module to the test server.
async fn publish_go_module(
    client: &reqwest::Client,
    base_url: &str,
    module_name: &str,
    version: &str,
) {
    let zip_data = build_go_module_zip(module_name, version);

    let resp = client
        .put(format!(
            "{}/go-hosted/{}/@v/{}",
            base_url, module_name, version
        ))
        .bearer_auth("test-token")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Publish a module, then list versions.
#[tokio::test]
async fn test_go_publish_and_list() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish v1.0.0
    publish_go_module(&client, &base_url, "mymodule", "v1.0.0").await;

    // Publish v1.1.0
    publish_go_module(&client, &base_url, "mymodule", "v1.1.0").await;

    // List versions
    let resp = client
        .get(format!("{}/go-hosted/mymodule/@v/list", base_url))
        .send()
        .await
        .expect("list request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read body");
    let versions: Vec<&str> = body.lines().collect();
    assert!(
        versions.contains(&"v1.0.0"),
        "version list should contain v1.0.0, got: {:?}",
        versions
    );
    assert!(
        versions.contains(&"v1.1.0"),
        "version list should contain v1.1.0, got: {:?}",
        versions
    );
    assert_eq!(versions.len(), 2, "should have exactly 2 versions");
}

/// 2. Publish, download zip, verify content.
#[tokio::test]
async fn test_go_download_zip() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let module_name = "dlmodule";
    let version = "v1.0.0";
    let original_zip = build_go_module_zip(module_name, version);

    // Publish
    let resp = client
        .put(format!(
            "{}/go-hosted/{}/@v/{}",
            base_url, module_name, version
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/zip")
        .body(original_zip.clone())
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Download zip
    let resp = client
        .get(format!(
            "{}/go-hosted/{}/@v/{}.zip",
            base_url, module_name, version
        ))
        .send()
        .await
        .expect("download request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let downloaded = resp.bytes().await.expect("failed to read zip bytes");
    assert_eq!(
        downloaded.as_ref(),
        original_zip.as_slice(),
        "downloaded zip should match the original"
    );
}

/// 3. Verify .info endpoint returns version info.
#[tokio::test]
async fn test_go_version_info() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    publish_go_module(&client, &base_url, "infomodule", "v2.0.0").await;

    // Get version info
    let resp = client
        .get(format!(
            "{}/go-hosted/infomodule/@v/v2.0.0.info",
            base_url
        ))
        .send()
        .await
        .expect("version info request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let info: Value = resp.json().await.expect("invalid json");
    assert_eq!(
        info["Version"], "v2.0.0",
        "version info should contain the correct version"
    );
    assert!(
        info["Time"].is_string(),
        "version info should contain a Time field"
    );
}
