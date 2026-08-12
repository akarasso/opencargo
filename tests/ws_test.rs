//! Real-time WebSocket tests (`src/api/ws.rs`, `GET /api/v1/events/ws`).
//!
//! Covers first-frame authentication (valid token, bad token, anonymous,
//! non-auth first frame), event delivery after a publish, visibility
//! filtering (private-repo `package.published` reaches admins only while
//! authenticated users get the anonymized `registry.changed` hint), and the
//! application-level ping/pong keepalive.
//!
//! NOT covered (too slow for the suite, noted deliberately):
//! - the protocol-level server Ping fires every 30s (tungstenite answers
//!   protocol Pings transparently anyway);
//! - the token revalidation runs every ~5 minutes;
//! - the 5s auth-frame timeout (a real 5s wait per test run).
//!
//! Non-receipt of filtered events is asserted through ordering: the broadcast
//! bus preserves emission order per subscriber, so if the frame received
//! right after publishing into the private repo is the *public* repo's event,
//! the private one was filtered — no flaky sleep-and-assert-nothing needed.

use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};

use opencargo::config::{
    AdminConfig, AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility, VulnScanConfig,
};
use opencargo::server;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_UNAUTHORIZED: u16 = 4401;

// ---------------------------------------------------------------------------
// Server harness (same shape as tests/authz_test.rs)
// ---------------------------------------------------------------------------

/// Start a server with one public repo (`npm-pub`) and one private repo
/// (`npm-secret`). `anonymous_read` drives whether the WS accepts anonymous
/// connections at Public visibility.
async fn setup(anonymous_read: bool) -> (String, tokio::task::JoinHandle<()>, TempDir) {
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
            anonymous_read,
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
    let base_url = format!("http://{addr}");

    config.server.base_url = base_url.clone();

    let state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let router = server::build_router(state);

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Poll with the admin token: /health/live sits inside the auth layer, so
    // an unauthenticated poll would 401 when anonymous_read is false.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client
            .get(format!("{base_url}/health/live"))
            .bearer_auth("test-token")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

    (base_url, handle, tmp)
}

// ---------------------------------------------------------------------------
// REST helpers (copied from tests/authz_test.rs)
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

async fn publish_package(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    repo: &str,
    package: &str,
    version: &str,
) {
    let pkg_json = format!(
        r#"{{"name":"{package}","version":"{version}","description":"ws test","main":"index.js"}}"#
    );
    let tarball = build_tarball(&pkg_json);
    let body = build_publish_body(package, version, "ws test", &tarball);

    let resp = client
        .put(format!("{base_url}/{repo}/{package}"))
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

async fn create_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    role: &str,
) {
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
}

async fn create_token_for_user(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    token_name: &str,
) -> String {
    let resp = client
        .post(format!("{base_url}/api/v1/users/{username}/tokens"))
        .bearer_auth(admin_token)
        .json(&json!({ "name": token_name }))
        .send()
        .await
        .expect("create token request failed");
    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid json from create token");
    assert_eq!(status, StatusCode::CREATED, "create token failed: {body:?}");
    body["token"]
        .as_str()
        .expect("token should be returned")
        .to_string()
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

async fn ws_connect(base_url: &str) -> Ws {
    let ws_url = format!("{}/api/v1/events/ws", base_url.replacen("http", "ws", 1));
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("ws connect failed");
    ws
}

async fn recv_frame(ws: &mut Ws) -> Message {
    tokio::time::timeout(RECV_TIMEOUT, ws.next())
        .await
        .expect("timed out waiting for a ws frame")
        .expect("ws stream ended without a close frame")
        .expect("ws protocol error")
}

/// Next text frame parsed as JSON, skipping protocol Ping/Pong frames.
async fn recv_json(ws: &mut Ws) -> Value {
    loop {
        match recv_frame(ws).await {
            Message::Text(t) => return serde_json::from_str(&t).expect("invalid json ws frame"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

async fn send_json(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string()))
        .await
        .expect("ws send failed");
}

/// Expect the server to close the connection with the given app close code.
async fn expect_close(ws: &mut Ws, code: u16) {
    loop {
        match recv_frame(ws).await {
            Message::Close(Some(frame)) => {
                assert_eq!(
                    u16::from(frame.code),
                    code,
                    "unexpected close code (reason: {})",
                    frame.reason
                );
                return;
            }
            Message::Close(None) => panic!("server closed without a code, expected {code}"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Send the first-frame auth message and return the server's reply frame.
async fn ws_auth(ws: &mut Ws, token: Option<&str>) -> Value {
    let frame = match token {
        Some(t) => json!({"type": "auth", "token": t}),
        None => json!({"type": "auth"}),
    };
    send_json(ws, frame).await;
    recv_json(ws).await
}

/// The event-bus subscription is created just *after* the hello frame is
/// sent; give the server task a beat so a publish right after the hello is
/// guaranteed to be observed by this connection.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

// ---------------------------------------------------------------------------
// (a) valid token → hello, then receives events / (d) admin sees private
// ---------------------------------------------------------------------------

/// A connection authenticated with a valid admin token gets the hello frame
/// and then the full `package.published` event for a *private* repo, followed
/// by the anonymized `registry.changed` hint (Authenticated ≤ Admin, so the
/// admin receives both).
#[tokio::test]
async fn test_ws_admin_auth_and_private_repo_events() {
    let (base_url, _handle, _tmp) = setup(true).await;
    let client = reqwest::Client::new();

    let mut ws = ws_connect(&base_url).await;
    let hello = ws_auth(&mut ws, Some("test-token")).await;
    assert_eq!(hello["type"], "hello", "expected hello, got {hello:?}");
    assert_eq!(hello["role"], "admin");
    assert_eq!(hello["anonymous"], false);
    settle().await;

    publish_package(&client, &base_url, "test-token", "npm-secret", "@sec/hidden", "1.0.0").await;

    let ev = recv_json(&mut ws).await;
    assert_eq!(ev["type"], "package.published", "got {ev:?}");
    assert_eq!(ev["data"]["package"], "@sec/hidden");
    assert_eq!(ev["data"]["repository"], "npm-secret");
    assert!(ev["ts"].is_string(), "events carry an RFC 3339 timestamp");

    let hint = recv_json(&mut ws).await;
    assert_eq!(hint["type"], "registry.changed", "got {hint:?}");
    assert_eq!(hint["data"]["repository"], "npm-secret");
}

// ---------------------------------------------------------------------------
// (b) bad token → close 4401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ws_bad_token_closed_4401() {
    let (base_url, _handle, _tmp) = setup(true).await;

    let mut ws = ws_connect(&base_url).await;
    send_json(&mut ws, json!({"type": "auth", "token": "not-a-valid-token"})).await;
    expect_close(&mut ws, CLOSE_UNAUTHORIZED).await;
}

/// A first frame that is not an auth frame (or not JSON at all) is rejected
/// with 4401 — the connection never reaches the event stream.
#[tokio::test]
async fn test_ws_non_auth_first_frame_closed_4401() {
    let (base_url, _handle, _tmp) = setup(true).await;

    // Well-formed JSON, wrong type.
    let mut ws = ws_connect(&base_url).await;
    send_json(&mut ws, json!({"type": "ping"})).await;
    expect_close(&mut ws, CLOSE_UNAUTHORIZED).await;

    // Not JSON at all.
    let mut ws = ws_connect(&base_url).await;
    ws.send(Message::text("definitely not json"))
        .await
        .expect("ws send failed");
    expect_close(&mut ws, CLOSE_UNAUTHORIZED).await;
}

// ---------------------------------------------------------------------------
// (c) anonymous: allowed at Public scope when anonymous_read, else 4401
// ---------------------------------------------------------------------------

/// With `anonymous_read = true`, an auth frame without a token yields an
/// anonymous identity at Public visibility: public-repo events are delivered,
/// while for a private-repo publish *neither* the Admin-level
/// `package.published` *nor* the Authenticated-level `registry.changed` hint
/// gets through. Ordering proves the filtering: the private publish happens
/// first, yet the first frame received is the public repo's event.
#[tokio::test]
async fn test_ws_anonymous_gets_public_scope_only() {
    let (base_url, _handle, _tmp) = setup(true).await;
    let client = reqwest::Client::new();

    let mut ws = ws_connect(&base_url).await;
    let hello = ws_auth(&mut ws, None).await;
    assert_eq!(hello["type"], "hello", "expected hello, got {hello:?}");
    assert_eq!(hello["role"], "anonymous");
    assert_eq!(hello["anonymous"], true);
    settle().await;

    publish_package(&client, &base_url, "test-token", "npm-secret", "@sec/hidden", "1.0.0").await;
    publish_package(&client, &base_url, "test-token", "npm-pub", "@pub/open", "1.0.0").await;

    let ev = recv_json(&mut ws).await;
    assert_eq!(
        ev["type"], "package.published",
        "anonymous must not have received any private-repo frame first, got {ev:?}"
    );
    assert_eq!(ev["data"]["package"], "@pub/open");
    assert_eq!(ev["data"]["repository"], "npm-pub");
}

/// With `anonymous_read = false`, a token-less auth frame is refused outright.
#[tokio::test]
async fn test_ws_anonymous_rejected_when_anonymous_read_disabled() {
    let (base_url, _handle, _tmp) = setup(false).await;

    let mut ws = ws_connect(&base_url).await;
    send_json(&mut ws, json!({"type": "auth"})).await;
    expect_close(&mut ws, CLOSE_UNAUTHORIZED).await;
}

// ---------------------------------------------------------------------------
// (d) visibility filtering for a logged-in non-admin
// ---------------------------------------------------------------------------

/// A non-admin user does NOT receive the full `package.published` event of a
/// private repo — only the anonymized `registry.changed` hint (no package
/// name). Note: WS visibility is scoped by *role*, not by per-repo
/// permissions — this reader could read npm-secret through the reader role
/// default, yet still only gets the hint. Ordering again proves the
/// filtering: hint first, then the public repo's full event.
#[tokio::test]
async fn test_ws_reader_gets_hint_not_payload_for_private_repo() {
    let (base_url, _handle, _tmp) = setup(true).await;
    let client = reqwest::Client::new();

    create_user(&client, &base_url, "test-token", "watcher", "reader").await;
    let reader_token =
        create_token_for_user(&client, &base_url, "test-token", "watcher", "ws-token").await;

    let mut ws = ws_connect(&base_url).await;
    let hello = ws_auth(&mut ws, Some(&reader_token)).await;
    assert_eq!(hello["type"], "hello", "expected hello, got {hello:?}");
    assert_eq!(hello["role"], "reader");
    assert_eq!(hello["username"], "watcher");
    assert_eq!(hello["anonymous"], false);
    settle().await;

    publish_package(&client, &base_url, "test-token", "npm-secret", "@sec/hidden", "1.0.0").await;
    publish_package(&client, &base_url, "test-token", "npm-pub", "@pub/open", "1.0.0").await;

    // First frame after the private publish: the anonymized hint, never the
    // private package's full event.
    let hint = recv_json(&mut ws).await;
    assert_eq!(
        hint["type"], "registry.changed",
        "reader must not receive the private package.published, got {hint:?}"
    );
    assert_eq!(hint["data"]["repository"], "npm-secret");
    assert!(
        hint["data"].get("package").is_none(),
        "the hint must not leak the private package name: {hint:?}"
    );

    // Then the public repo's full event.
    let ev = recv_json(&mut ws).await;
    assert_eq!(ev["type"], "package.published", "got {ev:?}");
    assert_eq!(ev["data"]["package"], "@pub/open");
}

// ---------------------------------------------------------------------------
// (e) application-level keepalive
// ---------------------------------------------------------------------------

/// `{"type":"ping"}` is answered with `{"type":"pong"}`, and a garbage text
/// frame after auth is ignored without killing the connection. (The
/// protocol-level server Ping fires every 30s — not waited on here.)
#[tokio::test]
async fn test_ws_app_ping_pong_and_garbage_tolerance() {
    let (base_url, _handle, _tmp) = setup(true).await;

    let mut ws = ws_connect(&base_url).await;
    let hello = ws_auth(&mut ws, Some("test-token")).await;
    assert_eq!(hello["type"], "hello");

    // Garbage after auth is ignored — the connection must survive it.
    ws.send(Message::text("still not json"))
        .await
        .expect("ws send failed");

    send_json(&mut ws, json!({"type": "ping"})).await;
    let pong = recv_json(&mut ws).await;
    assert_eq!(pong["type"], "pong", "got {pong:?}");
}
