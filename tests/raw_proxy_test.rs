//! Raw proxy and group repositories: the cache, the digest an upstream
//! announces, the walk order, and the policy row a proxied read records.

mod common;

use std::collections::HashMap;

use reqwest::StatusCode;
use sha2::Digest as _;

// The Maven fake is a plain HTTP file server, which is all a raw upstream is.
use common::fake_upstream::maven::{self as fake, FakeMaven};
use common::{
    group, hosted, proxy, sentinel, spawn_server, wait_for_policy_rows, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};
use opencargo::policy::rules::PolicyConfig;

const PATH: &str = "dist/tool-1.0.tar.gz";

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

async fn spawn(upstream: &FakeMaven) -> TestServer {
    spawn_with(upstream, HashMap::new()).await
}

async fn spawn_with(upstream: &FakeMaven, policy: HashMap<String, PolicyConfig>) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("local", RepositoryFormat::Raw, Visibility::Public),
            proxy("remote", RepositoryFormat::Raw, &upstream.base_url),
            group("files", RepositoryFormat::Raw, &["local", "remote"]),
        ],
        policy,
        ..Default::default()
    })
    .await
}

async fn get(s: &TestServer, repo: &str, path: &str) -> reqwest::Response {
    reqwest::get(format!("{}/raw/{repo}/{path}", s.base_url)).await.unwrap()
}

async fn put(s: &TestServer, repo: &str, path: &str, body: &'static [u8]) {
    let resp = reqwest::Client::new()
        .put(format!("{}/raw/{repo}/{path}", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "PUT {repo}/{path}");
}

#[tokio::test]
async fn a_proxy_serves_its_upstream_once_and_then_its_cache() {
    let upstream = fake::start().await;
    upstream.put(PATH, b"upstream bytes".to_vec());
    let s = spawn(&upstream).await;

    let first = get(&s, "remote", PATH).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.bytes().await.unwrap().as_ref(), b"upstream bytes");

    let second = get(&s, "remote", PATH).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second.headers()["x-checksum-sha256"],
        sha256_hex(b"upstream bytes").as_str()
    );
    assert_eq!(upstream.count(PATH), 1, "the second read came from the cache");

    assert_eq!(get(&s, "remote", "dist/absent.bin").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_body_that_does_not_match_the_digest_its_headers_announce_is_refused() {
    let upstream = fake::start().await;
    upstream.put_with(
        PATH,
        b"upstream bytes".to_vec(),
        vec![("x-checksum-sha256", "a".repeat(64))],
    );
    let s = spawn(&upstream).await;
    assert_eq!(get(&s, "remote", PATH).await.status(), StatusCode::BAD_GATEWAY);

    upstream.put_with(
        PATH,
        b"upstream bytes".to_vec(),
        vec![("x-checksum-sha256", sha256_hex(b"upstream bytes"))],
    );
    let served = get(&s, "remote", PATH).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.bytes().await.unwrap().as_ref(), b"upstream bytes");
}

#[tokio::test]
async fn a_group_serves_its_first_member_that_holds_the_path() {
    let upstream = fake::start().await;
    upstream.put(PATH, b"upstream bytes".to_vec());
    upstream.put("dist/only-upstream.bin", b"remote only".to_vec());
    let s = spawn(&upstream).await;
    put(&s, "local", PATH, b"local bytes").await;

    let served = get(&s, "files", PATH).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.bytes().await.unwrap().as_ref(), b"local bytes", "local comes first");

    let fell_through = get(&s, "files", "dist/only-upstream.bin").await;
    assert_eq!(fell_through.status(), StatusCode::OK);
    assert_eq!(fell_through.bytes().await.unwrap().as_ref(), b"remote only");

    assert_eq!(get(&s, "files", "dist/nowhere.bin").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_group_stays_up_when_a_member_is_down() {
    let upstream = fake::start().await;
    let s = spawn(&upstream).await;
    put(&s, "local", PATH, b"local bytes").await;
    upstream.set_down(true);

    let served = get(&s, "files", PATH).await;
    assert_eq!(served.status(), StatusCode::OK, "a hosted member still answers");
    assert_eq!(
        get(&s, "files", "dist/nowhere.bin").await.status(),
        StatusCode::BAD_GATEWAY,
        "nothing found and a member failed"
    );
}

#[tokio::test]
async fn a_proxied_read_is_recorded_for_the_policy_report() {
    let upstream = fake::start().await;
    upstream.put(PATH, b"upstream bytes".to_vec());
    upstream.put("dist/sentinel.bin", b"sentinel".to_vec());
    let s = spawn_with(
        &upstream,
        HashMap::from([(
            "remote".to_string(),
            PolicyConfig {
                typosquat: true,
                ..Default::default()
            },
        )]),
    )
    .await;

    assert_eq!(get(&s, "remote", PATH).await.status(), StatusCode::OK);
    let rows = wait_for_policy_rows(&s, 1).await;
    assert_eq!(rows[0].format, "raw");
    assert_eq!(rows[0].name, PATH);
    assert_eq!(rows[0].version, None, "a raw path has no version");
    assert_eq!(rows[0].member_repo, "remote");
    assert_eq!(rows[0].date_source, "none");
    assert_eq!(rows[0].digest.as_deref(), Some(sha256_hex(b"upstream bytes").as_str()));

    let url = format!("{}/raw/remote/dist/sentinel.bin", s.base_url);
    let rows = sentinel(&s, &url, 2).await;
    let verdicts = common::policy_verdicts(&s).await;
    let (verdict, reason) = common::verdict_of(&verdicts, rows[1].id, "typosquat");
    assert_eq!(verdict, "not_applicable", "{reason}");
}
