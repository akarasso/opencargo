use std::sync::{Arc, Mutex};

use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AdminConfig, AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility, WebhookConfig,
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

/// Received webhook data: (headers map, body json)
#[derive(Clone, Debug)]
struct WebhookCall {
    headers: Vec<(String, String)>,
    body: Value,
}

async fn setup_webhook_receiver() -> (String, Arc<Mutex<Vec<WebhookCall>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();

    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::Json<Value>| {
                let r = received_clone.clone();
                async move {
                    let mut header_pairs = Vec::new();
                    for (key, value) in headers.iter() {
                        if let Ok(v) = value.to_str() {
                            header_pairs.push((key.to_string(), v.to_string()));
                        }
                    }
                    r.lock().unwrap().push(WebhookCall {
                        headers: header_pairs,
                        body: body.0,
                    });
                    axum::http::StatusCode::OK
                }
            },
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async { axum::serve(listener, app).await });

    (format!("http://{}/hook", addr), received)
}

async fn setup_with_webhooks(
    webhooks: Vec<WebhookConfig>,
) -> (String, tokio::task::JoinHandle<()>, TempDir) {
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
                name: "npm-dev".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
            RepositoryConfig {
                name: "npm-prod".to_string(),
                repo_type: RepositoryType::Hosted,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: None,
            },
        ],
        webhooks,
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
        match client
            .get(format!("{}/health/live", &base_url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_webhook_on_publish() {
    // 1. Start the webhook receiver
    let (webhook_url, received) = setup_webhook_receiver().await;

    // 2. Start the registry with webhook config
    let (base_url, _handle, _tmp) = setup_with_webhooks(vec![WebhookConfig {
        url: webhook_url,
        events: vec!["package.published".to_string()],
        secret: None,
    }])
    .await;

    let client = reqwest::Client::new();

    // 3. Publish a package
    let pkg_json = r#"{"name":"@test/webhook-pkg","version":"1.0.0","description":"Webhook test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/webhook-pkg", "1.0.0", "Webhook test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/webhook-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // 4. Wait for the webhook to be delivered (fire-and-forget uses tokio::spawn)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 5. Check the webhook receiver
    let calls = received.lock().unwrap();
    assert!(
        !calls.is_empty(),
        "webhook receiver should have received at least one call"
    );

    let call = &calls[0];
    assert_eq!(call.body["event"], "package.published");
    assert_eq!(call.body["data"]["package"], "@test/webhook-pkg");
    assert_eq!(call.body["data"]["version"], "1.0.0");
    assert_eq!(call.body["data"]["repository"], "npm-dev");
    assert!(
        call.body["timestamp"].as_str().is_some(),
        "timestamp should be present"
    );
}

#[tokio::test]
async fn test_webhook_signature() {
    let secret = "my-test-secret-key";

    // 1. Start the webhook receiver
    let (webhook_url, received) = setup_webhook_receiver().await;

    // 2. Start the registry with webhook config including secret
    let (base_url, _handle, _tmp) = setup_with_webhooks(vec![WebhookConfig {
        url: webhook_url,
        events: vec!["package.published".to_string()],
        secret: Some(secret.to_string()),
    }])
    .await;

    let client = reqwest::Client::new();

    // 3. Publish a package
    let pkg_json = r#"{"name":"@test/sig-pkg","version":"1.0.0","description":"Sig test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/sig-pkg", "1.0.0", "Sig test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/sig-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. Wait for webhook delivery
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 5. Check the webhook receiver
    let calls = received.lock().unwrap();
    assert!(!calls.is_empty(), "should have received a webhook call");

    let call = &calls[0];

    // 6. Verify X-Webhook-Signature header is present
    let sig_header = call
        .headers
        .iter()
        .find(|(k, _)| k == "x-webhook-signature")
        .expect("X-Webhook-Signature header should be present");

    assert!(
        sig_header.1.starts_with("sha256="),
        "signature should start with sha256="
    );

    // 7. Verify the signature is correct by computing HMAC-SHA256
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let body_bytes = serde_json::to_vec(&call.body).unwrap();
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(&body_bytes);
    let expected_sig = format!(
        "sha256={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );

    assert_eq!(sig_header.1, expected_sig, "HMAC signature should match");
}

#[tokio::test]
async fn test_webhook_event_filter() {
    // 1. Start the webhook receiver
    let (webhook_url, received) = setup_webhook_receiver().await;

    // 2. Configure webhook for ONLY "package.promoted"
    let (base_url, _handle, _tmp) = setup_with_webhooks(vec![WebhookConfig {
        url: webhook_url,
        events: vec!["package.promoted".to_string()],
        secret: None,
    }])
    .await;

    let client = reqwest::Client::new();

    // 3. Publish a package (should NOT trigger webhook)
    let pkg_json = r#"{"name":"@test/filter-pkg","version":"1.0.0","description":"Filter test","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_publish_body("@test/filter-pkg", "1.0.0", "Filter test", &tarball);

    let resp = client
        .put(format!("{}/npm-dev/@test/filter-pkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. Wait and verify NO webhook was sent
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    {
        let calls = received.lock().unwrap();
        assert!(
            calls.is_empty(),
            "webhook should NOT have been called for package.published event (filter is package.promoted only)"
        );
    }

    // 5. Promote the package (should trigger webhook)
    let resp = client
        .post(format!(
            "{}/api/v1/promote/@test/filter-pkg/1.0.0",
            base_url
        ))
        .bearer_auth("test-token")
        .json(&json!({
            "from": "npm-dev",
            "to": "npm-prod"
        }))
        .send()
        .await
        .expect("promote request failed");
    assert_eq!(resp.status(), StatusCode::OK, "promote should succeed");

    // 6. Wait and verify webhook WAS called
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    {
        let calls = received.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "webhook should have been called exactly once (for promote)"
        );
        assert_eq!(calls[0].body["event"], "package.promoted");
        assert_eq!(calls[0].body["data"]["package"], "@test/filter-pkg");
    }
}
