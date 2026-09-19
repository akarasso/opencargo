//! The raw format over HTTP: PUT, GET, DELETE on a hosted repository, and
//! what the permission matrix makes of each.

mod common;

use reqwest::StatusCode;
use serde_json::json;
use sha2::Digest as _;

use common::{
    basic_auth_header, hosted, named_token, seed_error, spawn_server, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const PATH: &str = "dist/linux-amd64/tool-1.2.3.tar.gz";

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

async fn server(visibility: Visibility) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![hosted("files", RepositoryFormat::Raw, visibility)],
        ..Default::default()
    })
    .await
}

fn url(s: &TestServer, path: &str) -> String {
    format!("{}/raw/files/{path}", s.base_url)
}

async fn put(s: &TestServer, path: &str, body: &'static [u8]) -> reqwest::Response {
    reqwest::Client::new()
        .put(url(s, path))
        .bearer_auth(STATIC_TOKEN)
        .body(body)
        .send()
        .await
        .unwrap()
}

/// One grant on `files`, as the admin API writes it.
async fn grant(s: &TestServer, user: &str, write: bool, delete: bool) {
    let resp = reqwest::Client::new()
        .put(format!("{}/api/v1/users/{user}/permissions/files", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({
            "can_read": true,
            "can_write": write,
            "can_delete": delete,
            "can_admin": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "grant on files");
}

async fn get(s: &TestServer, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(url(s, path))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_repository_named_raw_is_refused_at_the_seed_and_by_the_api() {
    let err = seed_error(vec![hosted("raw", RepositoryFormat::Npm, Visibility::Public)]).await;
    assert!(err.contains("reserved"), "{err}");

    let server = spawn_server(SpawnOpts::default()).await;
    let client = reqwest::Client::new();
    let create = |name: &'static str| {
        client
            .post(format!("{}/api/v1/repositories", server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .json(&json!({"name": name, "type": "hosted", "format": "raw"}))
            .send()
    };
    let refused = create("raw").await.unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let created = create("build-artifacts").await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED, "{:?}", created.text().await);
}

#[tokio::test]
async fn a_file_is_put_read_back_with_its_checksum_and_deleted() {
    let s = server(Visibility::Public).await;
    let body: &[u8] = b"tarball bytes";
    let sha = sha256_hex(body);

    let stored = put(&s, PATH, b"tarball bytes").await;
    assert_eq!(stored.status(), StatusCode::CREATED);
    assert_eq!(stored.headers()["x-checksum-sha256"], sha.as_str());

    let served = get(&s, PATH).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.headers()["x-checksum-sha256"], sha.as_str());
    assert_eq!(served.headers()["etag"], format!("\"{sha}\"").as_str());
    assert_eq!(served.headers()["content-length"], "13");
    assert_eq!(served.bytes().await.unwrap().as_ref(), body);

    assert_eq!(get(&s, "dist/absent.bin").await.status(), StatusCode::NOT_FOUND);

    let removed = reqwest::Client::new()
        .delete(url(&s, PATH))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(get(&s, PATH).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_content_type_the_client_sent_is_the_one_served() {
    let s = server(Visibility::Public).await;
    let stored = reqwest::Client::new()
        .put(url(&s, "notes/readme.txt"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", "text/plain; charset=utf-8")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::CREATED);
    let served = get(&s, "notes/readme.txt").await;
    assert_eq!(served.headers()["content-type"], "text/plain; charset=utf-8");

    put(&s, "dist/blob.bin", b"bytes").await;
    let served = get(&s, "dist/blob.bin").await;
    assert_eq!(served.headers()["content-type"], "application/octet-stream");
}

#[tokio::test]
async fn one_path_holds_one_file_and_the_same_bytes_are_unchanged() {
    let s = server(Visibility::Public).await;
    assert_eq!(put(&s, PATH, b"first").await.status(), StatusCode::CREATED);
    assert_eq!(put(&s, PATH, b"first").await.status(), StatusCode::OK, "the same bytes");
    assert_eq!(put(&s, PATH, b"second").await.status(), StatusCode::CREATED, "a replacement");
    let served = get(&s, PATH).await;
    assert_eq!(served.headers()["x-checksum-sha256"], sha256_hex(b"second").as_str());
    assert_eq!(served.bytes().await.unwrap().as_ref(), b"second");
}

#[tokio::test]
async fn a_declared_checksum_is_checked_before_anything_is_recorded() {
    let s = server(Visibility::Public).await;
    let client = reqwest::Client::new();
    let refused = client
        .put(url(&s, PATH))
        .bearer_auth(STATIC_TOKEN)
        .header("x-checksum-sha256", "a".repeat(64))
        .body("tarball bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get(&s, PATH).await.status(), StatusCode::NOT_FOUND);

    let malformed = client
        .put(url(&s, PATH))
        .bearer_auth(STATIC_TOKEN)
        .header("x-checksum-sha256", "not-a-digest")
        .body("tarball bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

    let accepted = client
        .put(url(&s, PATH))
        .bearer_auth(STATIC_TOKEN)
        .header("x-checksum-sha256", sha256_hex(b"tarball bytes"))
        .body("tarball bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_path_that_climbs_out_of_the_repository_is_refused() {
    let s = server(Visibility::Public).await;
    let client = reqwest::Client::new();
    for path in ["../secret", "dist/../../secret", "_drafts/x", "dist//x"] {
        let refused = client
            .put(format!("{}/raw/files/{path}", s.base_url))
            .bearer_auth(STATIC_TOKEN)
            .body("x")
            .send()
            .await
            .unwrap();
        assert!(
            refused.status() == StatusCode::BAD_REQUEST || refused.status() == StatusCode::NOT_FOUND,
            "PUT {path}: {}",
            refused.status()
        );
    }
}

#[tokio::test]
async fn writing_and_deleting_follow_the_permission_matrix() {
    let s = server(Visibility::Private).await;
    let client = reqwest::Client::new();
    let reader = named_token(&client, &s.base_url, "reader-user", "ci").await;
    let publisher = named_token(&client, &s.base_url, "publisher-user", "ci").await;
    grant(&s, "publisher-user", true, false).await;
    grant(&s, "reader-user", false, false).await;

    put(&s, PATH, b"payload").await;

    let denied = client
        .put(url(&s, "dist/other.bin"))
        .bearer_auth(&reader)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN, "a reader may not write");

    let allowed = client
        .put(url(&s, "dist/other.bin"))
        .bearer_auth(&publisher)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::CREATED);

    let refused = client
        .delete(url(&s, "dist/other.bin"))
        .bearer_auth(&publisher)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN, "write does not carry delete");

    grant(&s, "publisher-user", true, true).await;
    let removed = client
        .delete(url(&s, "dist/other.bin"))
        .bearer_auth(&publisher)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);

    let anonymous = reqwest::get(url(&s, PATH)).await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED, "a private repository");
    assert_eq!(
        anonymous.headers()["www-authenticate"],
        "Basic realm=\"opencargo\"",
        "clients send credentials once challenged"
    );

    let by_basic = client
        .get(url(&s, PATH))
        .header("authorization", basic_auth_header("reader-user", &reader))
        .send()
        .await
        .unwrap();
    assert_eq!(by_basic.status(), StatusCode::OK, "a token is a Basic password");
}

#[tokio::test]
async fn a_hosted_write_route_refuses_a_proxy_and_a_repository_of_another_format() {
    let s = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            common::proxy("files-proxy", RepositoryFormat::Raw, "http://127.0.0.1:1/"),
        ],
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let wrong_format = client
        .put(format!("{}/raw/npm-hosted/dist/x.bin", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_format.status(), StatusCode::BAD_REQUEST);

    let into_proxy = client
        .put(format!("{}/raw/files-proxy/dist/x.bin", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(into_proxy.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_repository_holding_files_cannot_be_deleted_until_they_are() {
    let s = server(Visibility::Public).await;
    put(&s, PATH, b"payload").await;
    let client = reqwest::Client::new();
    let held = client
        .delete(format!("{}/api/v1/repositories/files", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::CONFLICT);

    client
        .delete(url(&s, PATH))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    let gone = client
        .delete(format!("{}/api/v1/repositories/files", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert!(gone.status().is_success(), "{}", gone.status());
}
