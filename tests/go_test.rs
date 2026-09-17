mod common;

use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use common::{build_go_module_zip, hosted, publish_go_module, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Start a test server on a random port with a Go hosted repository.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted("go-hosted", RepositoryFormat::Go, Visibility::Public)],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
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
    publish_go_module(&client, &base_url, "go-hosted", "mymodule", "v1.0.0").await;

    // Publish v1.1.0
    publish_go_module(&client, &base_url, "go-hosted", "mymodule", "v1.1.0").await;

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

    publish_go_module(&client, &base_url, "go-hosted", "infomodule", "v2.0.0").await;

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

/// 4. Multi-segment module paths (the realistic case: `example.com/org/repo`).
/// axum's `{module}` route param matches a single URL segment, so these go
/// through the `/{repo}/{*rest}` wildcard dispatch: publish, @v/list,
/// .info, .zip download and @latest must all resolve.
#[tokio::test]
async fn test_go_multi_segment_module() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let module = "example.com/org/repo";

    publish_go_module(&client, &base_url, "go-hosted", module, "v1.0.0").await;

    // Publish v1.1.0 by hand, keeping the zip bytes for the download check.
    let zip_v110 = build_go_module_zip(module, "v1.1.0");
    let resp = client
        .put(format!("{}/go-hosted/{}/@v/v1.1.0", base_url, module))
        .bearer_auth("test-token")
        .header("content-type", "application/zip")
        .body(zip_v110.clone())
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "multi-segment publish failed: {:?}",
        resp.text().await
    );

    // @v/list
    let resp = client
        .get(format!("{}/go-hosted/{}/@v/list", base_url, module))
        .send()
        .await
        .expect("list request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("failed to read body");
    let versions: Vec<&str> = body.lines().collect();
    assert!(versions.contains(&"v1.0.0"), "list should contain v1.0.0, got {versions:?}");
    assert!(versions.contains(&"v1.1.0"), "list should contain v1.1.0, got {versions:?}");

    // .info
    let resp = client
        .get(format!("{}/go-hosted/{}/@v/v1.0.0.info", base_url, module))
        .send()
        .await
        .expect("info request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let info: Value = resp.json().await.expect("invalid json");
    assert_eq!(info["Version"], "v1.0.0");

    // .zip download must return the exact published bytes
    let resp = client
        .get(format!("{}/go-hosted/{}/@v/v1.1.0.zip", base_url, module))
        .send()
        .await
        .expect("download request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let downloaded = resp.bytes().await.expect("failed to read zip bytes");
    assert_eq!(
        downloaded.as_ref(),
        zip_v110.as_slice(),
        "downloaded zip should match the original"
    );

    // @latest returns the most recently published version
    let resp = client
        .get(format!("{}/go-hosted/{}/@latest", base_url, module))
        .send()
        .await
        .expect("latest request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let latest: Value = resp.json().await.expect("invalid json");
    assert_eq!(latest["Version"], "v1.1.0", "@latest should be the last published version");
    assert!(latest["Time"].is_string(), "@latest should carry a Time field");
}

/// 5. Hostile module names must be rejected with 400 before touching storage:
/// an empty path segment (`a//b`, wildcard route) and a forbidden character
/// (`bad!mod`, single-segment route).
#[tokio::test]
async fn test_go_publish_rejects_invalid_module_names() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    for bad_module in ["a//b", "bad!mod"] {
        let zip_data = build_go_module_zip("whatever", "v1.0.0");
        let resp = client
            .put(format!("{}/go-hosted/{}/@v/v1.0.0", base_url, bad_module))
            .bearer_auth("test-token")
            .header("content-type", "application/zip")
            .body(zip_data)
            .send()
            .await
            .expect("publish request failed");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "module name '{bad_module}' must be rejected with 400"
        );
    }

    // Invalid version too: `%252F` survives the server's `%2F` decoding and
    // reaches the handler as a version containing a slash.
    let zip_data = build_go_module_zip("okmod", "v1.0.0");
    let resp = client
        .put(format!("{}/go-hosted/okmod/@v/v1.0%252F0", base_url))
        .bearer_auth("test-token")
        .header("content-type", "application/zip")
        .body(zip_data)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a version containing a slash must be rejected with 400"
    );
}

/// 6. B19 regression: a Go module whose go.mod exceeds the 1 MiB cap is
/// rejected at publish (400 "go.mod entry too large") rather than read fully
/// into memory — the zip-bomb defense in extract_go_mod_from_zip. We assert on
/// the declared-size guard (file.size() > MAX), which fires deterministically.
#[tokio::test]
async fn test_go_oversized_gomod_rejected() {
    use std::io::Write;

    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let module_name = "bigmod";
    let version = "v1.0.0";

    // Build a zip whose go.mod is larger than the 1 MiB cap.
    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let go_mod_path = format!("{}@{}/go.mod", module_name, version);
        zip_writer.start_file(&go_mod_path, options).unwrap();
        let mut content = format!("module {}\n\ngo 1.21\n", module_name);
        while content.len() <= 1024 * 1024 {
            content.push_str("// padding to exceed the 1 MiB go.mod cap\n");
        }
        zip_writer.write_all(content.as_bytes()).unwrap();
        zip_writer.finish().unwrap();
    }

    let resp = client
        .put(format!(
            "{}/go-hosted/{}/@v/{}",
            base_url, module_name, version
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/zip")
        .body(buf)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an oversized go.mod must be rejected with 400"
    );
    let text = resp.text().await.unwrap_or_default();
    assert!(
        text.contains("too large"),
        "400 body should mention the go.mod is too large, got: {text}"
    );
}
