use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::Digest;
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, Visibility,
};
use opencargo::server;

/// Start a test server on a random port with an OCI hosted repository.
///
/// Returns the base URL, the join handle for the server task, and the TempDir
/// (kept alive so the directory is not deleted while tests are running).
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
            name: "oci-private".to_string(),
            repo_type: RepositoryType::Hosted,
            format: RepositoryFormat::Oci,
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
    let base_url = format!("http://{}", addr);

    config.server.base_url = base_url.clone();

    let state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let router = server::build_router(state);

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Wait for the server to be ready.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{}/health/live", &base_url)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

/// Compute sha256 digest in the OCI format "sha256:hex..."
fn sha256_digest(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Helper: upload a blob (monolithic PUT) and return the digest.
async fn push_blob(client: &reqwest::Client, base_url: &str, blob_data: &[u8]) -> String {
    let digest = sha256_digest(blob_data);

    // Step 1: Initiate upload
    let resp = client
        .post(format!(
            "{}/v2/oci-private/myapp/blobs/uploads/",
            base_url
        ))
        .bearer_auth("test-token")
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

    // Step 2: Complete upload with PUT
    let upload_url = format!("{}{}?digest={}", base_url, location, digest);
    let resp = client
        .put(&upload_url)
        .bearer_auth("test-token")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_oci_version_check() {
    let (base_url, _handle, _tmp) = setup().await;

    let resp = reqwest::get(format!("{}/v2/", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn test_oci_push_and_pull() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create test data: a config blob and a layer blob
    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"fake-layer-data-for-testing-purposes";

    // Push the config blob
    let config_digest = push_blob(&client, &base_url, config_data).await;

    // Push the layer blob
    let layer_digest = push_blob(&client, &base_url, layer_data).await;

    // Build a manifest
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = sha256_digest(&manifest_bytes);

    // Push the manifest with tag "latest"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "put manifest failed: {:?}",
        resp.text().await
    );

    // Pull the manifest by tag
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .send()
        .await
        .expect("get manifest request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header")
        .to_string();
    assert_eq!(pulled_digest, manifest_digest);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type")
        .to_str()
        .expect("invalid content-type")
        .to_string();
    assert_eq!(
        content_type,
        "application/vnd.oci.image.manifest.v1+json"
    );

    let pulled_manifest: Value = resp.json().await.expect("invalid json");
    assert_eq!(pulled_manifest, manifest);

    // Pull the layer blob
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/blobs/{}",
            base_url, layer_digest
        ))
        .send()
        .await
        .expect("get blob request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_blob = resp.bytes().await.expect("failed to read blob");
    assert_eq!(pulled_blob.as_ref(), layer_data);
}

#[tokio::test]
async fn test_oci_list_tags() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Push some blobs and a manifest with multiple tags
    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"layer-data-for-tagging-test";

    let config_digest = push_blob(&client, &base_url, config_data).await;
    let layer_digest = push_blob(&client, &base_url, layer_data).await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    // Push with tag "v1.0"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/v1.0",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Push with tag "latest"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List tags
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/tags/list",
            base_url
        ))
        .send()
        .await
        .expect("list tags request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    let tags = body["tags"]
        .as_array()
        .expect("tags should be an array");

    assert_eq!(tags.len(), 2, "expected 2 tags, got {:?}", tags);
    let tag_names: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
    assert!(
        tag_names.contains(&"v1.0"),
        "tags should contain v1.0: {:?}",
        tag_names
    );
    assert!(
        tag_names.contains(&"latest"),
        "tags should contain latest: {:?}",
        tag_names
    );
}

#[tokio::test]
async fn test_oci_head_blob() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let blob_data = b"test-blob-content-for-head-check";
    let digest = push_blob(&client, &base_url, blob_data).await;

    // HEAD the blob (anonymous read includes HEAD)
    let resp = client
        .head(format!(
            "{}/v2/oci-private/myapp/blobs/{}",
            base_url, digest
        ))
        .send()
        .await
        .expect("head blob request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify headers
    let content_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header");
    assert_eq!(content_digest, digest);

    let content_length = resp
        .headers()
        .get("content-length")
        .expect("missing Content-Length header")
        .to_str()
        .expect("invalid content-length");
    assert_eq!(
        content_length,
        blob_data.len().to_string(),
        "content-length mismatch"
    );

    // HEAD a non-existent blob should return 404
    let resp = client
        .head(format!(
            "{}/v2/oci-private/myapp/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
            base_url
        ))
        .send()
        .await
        .expect("head blob request failed");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_oci_manifest_by_digest() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"layer-data-for-digest-test";

    let config_digest = push_blob(&client, &base_url, config_data).await;
    let layer_digest = push_blob(&client, &base_url, layer_data).await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = sha256_digest(&manifest_bytes);

    // Push manifest with tag "v2.0"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/v2.0",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Retrieve the manifest by its digest (not by tag)
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/manifests/{}",
            base_url, manifest_digest
        ))
        .send()
        .await
        .expect("get manifest by digest request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header")
        .to_string();
    assert_eq!(pulled_digest, manifest_digest);

    let pulled_manifest: Value = resp.json().await.expect("invalid json");
    assert_eq!(pulled_manifest, manifest);
}
