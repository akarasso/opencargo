//! The surfaces that serve a stored column verbatim, pinned as each row type
//! is retyped: most of them served SQLite's `YYYY-MM-DD HH:MM:SS` and must
//! serve RFC 3339, while a hosted repository's absent config must keep
//! serving JSON `null`.

mod common;

use reqwest::StatusCode;
use serde_json::Value;

use common::{build_npm_publish_body, build_tarball, hosted, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const REPO: &str = "npm-hosted";
const PKG: &str = "@acme/widget";

async fn server() -> common::TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![hosted(REPO, RepositoryFormat::Npm, Visibility::Public)],
        ..Default::default()
    })
    .await
}

fn rfc3339(label: &str, value: &Value) {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("{label} is not a string: {value}"));
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("{label} is not RFC 3339: {text:?} ({e})"));
}

/// The packument's `time` map is built from `packages.created_at`,
/// `packages.updated_at` and every `versions.published_at`; npm clients read
/// those as timestamps, and nothing in the suite pinned their format.
#[tokio::test]
async fn hosted_packument_times_are_rfc_3339() {
    let srv = server().await;
    let client = reqwest::Client::new();

    let tarball = build_tarball(r#"{"name":"@acme/widget","version":"1.0.0"}"#);
    let body = build_npm_publish_body(PKG, "1.0.0", "a widget", &tarball);
    let resp = client
        .put(format!("{}/{REPO}/{PKG}", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);

    let packument: Value = client
        .get(format!("{}/{REPO}/{PKG}", srv.base_url))
        .send()
        .await
        .expect("packument request failed")
        .json()
        .await
        .expect("packument is not json");

    let time = packument["time"]
        .as_object()
        .expect("packument has no time map");
    assert!(
        time.contains_key("created") && time.contains_key("modified") && time.contains_key("1.0.0"),
        "time map is incomplete: {time:?}"
    );
    for (key, value) in time {
        rfc3339(&format!("time[{key}]"), value);
    }
}

/// `GET /api/v1/repositories/{name}` serves `repositories.created_at` and
/// `updated_at` straight out of the column.
#[tokio::test]
async fn repository_timestamps_are_rfc_3339() {
    let srv = server().await;

    let repo: Value = reqwest::Client::new()
        .get(format!("{}/api/v1/repositories/{REPO}", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("repository request failed")
        .json()
        .await
        .expect("repository is not json");

    rfc3339("created_at", &repo["created_at"]);
    rfc3339("updated_at", &repo["updated_at"]);
}

/// `config_json` is NULL on every hosted and every proxy repository -- the
/// majority of rows -- so `config` is JSON `null` there, and a config type
/// that cannot express "absent" would flip it to an empty member list.
#[tokio::test]
async fn a_hosted_repository_serves_a_null_config() {
    let srv = server().await;

    let repo: Value = reqwest::Client::new()
        .get(format!("{}/api/v1/repositories/{REPO}", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("repository request failed")
        .json()
        .await
        .expect("repository is not json");

    assert_eq!(repo["config"], Value::Null, "{repo}");
}

/// `GET /api/v1/users/{u}` serves `users.created_at` and `updated_at`, and
/// nothing in the suite pinned their format before `User` was retyped.
#[tokio::test]
async fn user_timestamps_are_rfc_3339() {
    let srv = server().await;
    let client = reqwest::Client::new();

    let created: Value = client
        .post(format!("{}/api/v1/users", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&serde_json::json!({"username": "wire", "role": "reader"}))
        .send()
        .await
        .expect("user creation failed")
        .json()
        .await
        .expect("created user is not json");
    rfc3339("created_at", &created["created_at"]);

    let user: Value = client
        .get(format!("{}/api/v1/users/wire", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("user request failed")
        .json()
        .await
        .expect("user is not json");

    rfc3339("created_at", &user["created_at"]);
    rfc3339("updated_at", &user["updated_at"]);
}

/// An API token's three timestamps reach the client the same way. The expiry
/// matters most: it used to be written and read back in one adapter's
/// spelling, on the path that rejects a credential.
#[tokio::test]
async fn token_timestamps_are_rfc_3339() {
    let srv = server().await;
    let client = reqwest::Client::new();

    client
        .post(format!("{}/api/v1/users", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&serde_json::json!({"username": "wire", "role": "reader"}))
        .send()
        .await
        .expect("user creation failed");

    let issued: Value = client
        .post(format!("{}/api/v1/users/wire/tokens", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&serde_json::json!({"name": "ci", "expires_in_days": 30}))
        .send()
        .await
        .expect("token creation failed")
        .json()
        .await
        .expect("issued token is not json");
    rfc3339("expires_at", &issued["expires_at"]);

    let listed: Value = client
        .get(format!("{}/api/v1/users/wire/tokens", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("token list failed")
        .json()
        .await
        .expect("token list is not json");
    let token = &listed[0];
    rfc3339("created_at", &token["created_at"]);
    rfc3339("expires_at", &token["expires_at"]);
    assert_eq!(
        token["last_used_at"],
        Value::Null,
        "an unused token has no last_used_at: {token}"
    );
}
