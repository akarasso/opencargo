mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{build_cargo_publish_body, build_crate_data, hosted, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};

/// Start a test server with a private cargo repository beside an npm one.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    setup_with(true).await
}

async fn setup_with(anonymous_read: bool) -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        anonymous_read,
        repositories: vec![
            hosted("test-npm", RepositoryFormat::Npm, Visibility::Public),
            hosted(
                "cargo-private",
                RepositoryFormat::Cargo,
                Visibility::Private,
            ),
        ],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
}

#[tokio::test]
async fn test_cargo_config_json() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/cargo-private/index/config.json", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("config.json request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let config: Value = resp.json().await.expect("invalid JSON");
    assert!(
        config.get("dl").is_some(),
        "config.json should have 'dl' field: {:?}",
        config
    );
    assert!(
        config.get("api").is_some(),
        "config.json should have 'api' field: {:?}",
        config
    );

    // Verify the dl URL contains the repo name
    let dl = config["dl"].as_str().expect("dl should be a string");
    assert!(
        dl.contains("cargo-private"),
        "dl URL should contain 'cargo-private': {}",
        dl
    );
}

/// cargo reads `config.json` before it knows whether to send a token and
/// learns to from `auth-required`, so the tokenless read must pass the
/// `anonymous_read = false` gate while disclosing only existence and format;
/// a sent token must still hold read, and the index stays gated.
#[tokio::test]
async fn config_json_tokenless_gate() {
    let (base_url, _handle, _tmp) = setup_with(false).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/cargo-private/index/config.json", base_url))
        .send()
        .await
        .expect("config.json request failed");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    let config: Value = resp.json().await.expect("invalid JSON");
    assert_eq!(
        config["dl"],
        format!("{}/cargo-private/api/v1/crates", base_url)
    );
    assert_eq!(config["api"], format!("{}/cargo-private", base_url));
    assert_eq!(config["auth-required"], true);

    let resp = client
        .get(format!("{}/test-npm/index/config.json", base_url))
        .send()
        .await
        .expect("npm config.json request failed");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = client
        .get(format!("{}/cargo-private/index/1/a", base_url))
        .send()
        .await
        .expect("index request failed");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the index stays gated"
    );

    let resp = client
        .post(format!("{}/api/v1/users", base_url))
        .bearer_auth("test-token")
        .json(&json!({"username": "no-read", "role": "reader"}))
        .send()
        .await
        .expect("create user failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = client
        .post(format!("{}/api/v1/users/no-read/tokens", base_url))
        .bearer_auth("test-token")
        .json(&json!({"name": "t"}))
        .send()
        .await
        .expect("create token failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let token: Value = resp.json().await.expect("invalid JSON");
    let token = token["token"].as_str().expect("token should be returned");
    let resp = client
        .put(format!("{}/api/v1/users/no-read/permissions/cargo-private", base_url))
        .bearer_auth("test-token")
        .json(&json!({"can_read": false, "can_write": false, "can_delete": false, "can_admin": false}))
        .send()
        .await
        .expect("set permission failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = client
        .get(format!("{}/cargo-private/index/config.json", base_url))
        .bearer_auth(token)
        .send()
        .await
        .expect("config.json request failed");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "{:?}",
        resp.text().await
    );
}

#[tokio::test]
async fn test_cargo_publish_and_download() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!("{}/cargo-private/api/v1/crates/new", base_url))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Download
    let resp = client
        .get(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/download",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("cargo download request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let downloaded = resp.bytes().await.expect("failed to read crate bytes");
    assert_eq!(
        downloaded.as_ref(),
        crate_data.as_slice(),
        "downloaded crate data should match the original"
    );
}

#[tokio::test]
async fn test_cargo_index_entry() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!("{}/cargo-private/api/v1/crates/new", base_url))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Fetch index entry: "test-crate" is 10 chars, prefix = te/st
    let resp = client
        .get(format!("{}/cargo-private/index/te/st/test-crate", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body_text = resp.text().await.expect("failed to read index body");

    // The index entry should contain a JSON line with name and vers
    assert!(
        body_text.contains(r#""name":"test-crate""#),
        "index entry should contain '\"name\":\"test-crate\"', got: {}",
        body_text
    );
    assert!(
        body_text.contains(r#""vers":"0.1.0""#),
        "index entry should contain '\"vers\":\"0.1.0\"', got: {}",
        body_text
    );
}

#[tokio::test]
async fn test_cargo_yank_unyank() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Test","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);

    // Publish
    let resp = client
        .put(format!("{}/cargo-private/api/v1/crates/new", base_url))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cargo publish failed: {:?}",
        resp.text().await
    );

    // Yank
    let resp = client
        .delete(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/yank",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("yank request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify index shows yanked:true
    let resp = client
        .get(format!("{}/cargo-private/index/te/st/test-crate", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request after yank failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body_text = resp.text().await.expect("failed to read index body");
    assert!(
        body_text.contains(r#""yanked":true"#),
        "index entry should contain '\"yanked\":true' after yank, got: {}",
        body_text
    );

    // Unyank
    let resp = client
        .put(format!(
            "{}/cargo-private/api/v1/crates/test-crate/0.1.0/unyank",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("unyank request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify index shows yanked:false
    let resp = client
        .get(format!("{}/cargo-private/index/te/st/test-crate", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("index entry request after unyank failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body_text = resp.text().await.expect("failed to read index body");
    assert!(
        body_text.contains(r#""yanked":false"#),
        "index entry should contain '\"yanked\":false' after unyank, got: {}",
        body_text
    );
}

/// Hostile crate names in the publish metadata must be rejected with 400
/// before touching DB or storage: the name is interpolated into the storage
/// path (`cargo/{repo}/{name}/{name}-{vers}.crate`).
#[tokio::test]
async fn test_cargo_publish_rejects_invalid_crate_name() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    for bad_meta in [
        // Path traversal in the name
        r#"{"name":"../evil","vers":"1.0.0","deps":[],"features":{},"authors":[],"description":"x"}"#,
        // Slash in the name -> arbitrary storage subtree
        r#"{"name":"a/b","vers":"1.0.0","deps":[],"features":{},"authors":[],"description":"x"}"#,
        // Version with a slash
        r#"{"name":"okcrate","vers":"1.0/0","deps":[],"features":{},"authors":[],"description":"x"}"#,
    ] {
        let body = build_cargo_publish_body(bad_meta, &build_crate_data());
        let resp = client
            .put(format!("{}/cargo-private/api/v1/crates/new", base_url))
            .bearer_auth("test-token")
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .expect("cargo publish request failed");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "hostile cargo metadata must be rejected with 400: {bad_meta}"
        );
    }
}
