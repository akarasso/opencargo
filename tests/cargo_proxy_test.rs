mod common;

use std::io::Write;

use reqwest::StatusCode;
use serde_json::Value;
use sha2::Digest;

use common::upstream_tap::{self, Tap};
use common::{
    build_cargo_publish_body, group, hosted, proxy_with, spawn_server, ProxyOpts, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};
use opencargo::registry::cargo::compute_prefix;

const UPSTREAM_REPO: &str = "cargo-up";
const CRATE: &str = "widget";

/// A second opencargo acting as the sparse upstream, fronted by a tap. Its
/// `dl` names its own address, so downloads bypass the tap and need
/// `dl_allow_private`.
struct Upstream {
    server: TestServer,
    tap: Tap,
}

impl Upstream {
    fn index_url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}/index", self.tap.base_url)
    }

    fn index_path(&self, name: &str) -> String {
        format!("/{UPSTREAM_REPO}/index/{}/{name}", compute_prefix(name))
    }
}

async fn spawn_upstream() -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            UPSTREAM_REPO,
            RepositoryFormat::Cargo,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    let tap = upstream_tap::start(&server.base_url).await;
    Upstream { server, tap }
}

fn proxy_repo(name: &str, up: &Upstream) -> RepositoryConfig {
    proxy_with(
        name,
        RepositoryFormat::Cargo,
        &up.index_url(),
        ProxyOpts {
            dl_allow_private: true,
            ..Default::default()
        },
    )
}

/// Distinct `.crate` bytes per seed, so two members holding the same
/// version are told apart by cksum.
fn crate_bytes(seed: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(seed.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn cksum(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// The smallest metadata JSON `cargo publish` sends that the server accepts.
fn publish_meta(name: &str, version: &str) -> String {
    format!(
        r#"{{"name":"{name}","vers":"{version}","deps":[],"features":{{}},"authors":[],"description":"d"}}"#
    )
}

/// Publish `name@version` into a hosted repository of `server`.
async fn publish(server: &TestServer, repo: &str, name: &str, version: &str, bytes: &[u8]) {
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/api/v1/crates/new", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", "application/octet-stream")
        .body(build_cargo_publish_body(&publish_meta(name, version), bytes))
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "seed publish should succeed");
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::get(url).await.expect("request failed")
}

async fn get_with_token(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("request failed")
}

fn index_url(server: &TestServer, repo: &str, name: &str) -> String {
    format!(
        "{}/{repo}/index/{}/{name}",
        server.base_url,
        compute_prefix(name)
    )
}

fn download_url(server: &TestServer, repo: &str, name: &str, version: &str) -> String {
    format!(
        "{}/{repo}/api/v1/crates/{name}/{version}/download",
        server.base_url
    )
}

/// The `(vers, cksum)` pairs of an index body, in order.
async fn index_entries(resp: reqwest::Response) -> Vec<(String, String)> {
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("failed to read index body");
    body.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("index line should be JSON");
            (
                v["vers"].as_str().unwrap().to_string(),
                v["cksum"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn download(url: &str) -> Vec<u8> {
    let resp = get(url).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {url}");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/x-tar")
    );
    resp.bytes().await.expect("failed to read body").to_vec()
}

/// `(kind, cache_key, status)` of every cache row the server holds.
async fn cache_rows(server: &TestServer) -> Vec<(String, String, i64)> {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT kind, cache_key, status FROM proxy_cache_entries ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read cache rows");
    pool.close().await;
    rows
}

/// A group over a hosted member holding `widget 0.2.0` and a proxy member
/// fronting an upstream holding `widget 0.1.0` and `0.2.0`.
struct Grouped {
    up: Upstream,
    a: TestServer,
    local_0_2: Vec<u8>,
    up_0_1: Vec<u8>,
    up_0_2: Vec<u8>,
}

async fn spawn_group() -> Grouped {
    let up = spawn_upstream().await;
    let (up_0_1, up_0_2, local_0_2) = (
        crate_bytes("up-0.1.0"),
        crate_bytes("up-0.2.0"),
        crate_bytes("local-0.2.0"),
    );
    publish(&up.server, UPSTREAM_REPO, CRATE, "0.1.0", &up_0_1).await;
    publish(&up.server, UPSTREAM_REPO, CRATE, "0.2.0", &up_0_2).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("cargo-local", RepositoryFormat::Cargo, Visibility::Public),
            proxy_repo("cargo-proxy", &up),
            group(
                "cargo-all",
                RepositoryFormat::Cargo,
                &["cargo-local", "cargo-proxy"],
            ),
        ],
        ..Default::default()
    })
    .await;
    publish(&a, "cargo-local", CRATE, "0.2.0", &local_0_2).await;
    Grouped {
        up,
        a,
        local_0_2,
        up_0_1,
        up_0_2,
    }
}

#[tokio::test]
async fn group_config_json_dl_points_at_group() {
    let g = spawn_group().await;
    let resp = get(&format!("{}/cargo-all/index/config.json", g.a.base_url)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let config: Value = resp.json().await.expect("invalid json");
    assert_eq!(
        config["dl"],
        format!("{}/cargo-all/api/v1/crates", g.a.base_url)
    );
    assert_eq!(config["api"], format!("{}/cargo-all", g.a.base_url));
    assert!(config.get("auth-required").is_none(), "{config}");
    assert_eq!(
        g.up.tap.hits.lock().unwrap().len(),
        0,
        "config.json is never proxied"
    );
}

#[tokio::test]
async fn group_index_unions_hosted_and_proxy_lines() {
    let g = spawn_group().await;
    let entries = index_entries(get(&index_url(&g.a, "cargo-all", CRATE)).await).await;
    assert_eq!(
        entries,
        vec![
            ("0.2.0".to_string(), cksum(&g.local_0_2)),
            ("0.1.0".to_string(), cksum(&g.up_0_1)),
        ],
        "hosted lines first, one line per version, the hosted 0.2.0 wins over the upstream's"
    );
    assert_ne!(cksum(&g.local_0_2), cksum(&g.up_0_2));
    assert_eq!(g.up.tap.count(&g.up.index_path(CRATE)), 1);
}

#[tokio::test]
async fn group_download_first_member_wins() {
    let g = spawn_group().await;
    let from_local = download(&download_url(&g.a, "cargo-all", CRATE, "0.2.0")).await;
    assert_eq!(from_local, g.local_0_2, "the hosted member answers first");
    let from_upstream = download(&download_url(&g.a, "cargo-all", CRATE, "0.1.0")).await;
    assert_eq!(
        from_upstream, g.up_0_1,
        "a version only upstream falls through to the proxy"
    );
    let rows = cache_rows(&g.a).await;
    assert!(
        rows.iter()
            .any(|(k, key, s)| k == "cargo-crate" && key == "widget/0.1.0" && *s == 200),
        "the proxied crate is cached under the member: {rows:?}"
    );
    assert!(
        !rows.iter().any(|(_, key, _)| key == "widget/0.2.0"),
        "the hosted hit never touched the upstream: {rows:?}"
    );
}

#[tokio::test]
async fn yank_on_group_is_400() {
    let g = spawn_group().await;
    let client = reqwest::Client::new();
    let base = format!("{}/cargo-all/api/v1/crates", g.a.base_url);
    for req in [
        client.delete(format!("{base}/{CRATE}/0.2.0/yank")),
        client.put(format!("{base}/{CRATE}/0.2.0/unyank")),
        client
            .put(format!("{base}/new"))
            .body(build_cargo_publish_body(&publish_meta(CRATE, "9.9.9"), b"x")),
    ] {
        let resp = req
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .expect("request failed");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "writes on a group are 400"
        );
    }
    let entries = index_entries(get(&index_url(&g.a, "cargo-local", CRATE)).await).await;
    assert_eq!(entries.len(), 1, "nothing was yanked or published");
}

#[tokio::test]
async fn group_index_degraded_member_warns_199() {
    let g = spawn_group().await;
    g.up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);

    let resp = get(&index_url(&g.a, "cargo-all", CRATE)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a hosted hit beside a failing member is still served"
    );
    let warning = resp
        .headers()
        .get("warning")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        warning.starts_with("199"),
        "the degradation is announced with Warning 199, got {warning:?}"
    );
    assert_eq!(
        index_entries(resp).await,
        vec![("0.2.0".to_string(), cksum(&g.local_0_2))],
        "only the hosted lines remain"
    );
    assert_eq!(
        g.up.tap.count(&g.up.index_path(CRATE)),
        1,
        "the proxy member was asked and got the 503"
    );
}

#[tokio::test]
async fn group_hides_private_member() {
    let up = spawn_upstream().await;
    let secret = crate_bytes("secret");
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("cargo-secret", RepositoryFormat::Cargo, Visibility::Private),
            proxy_repo("cargo-proxy", &up),
            group(
                "cargo-all",
                RepositoryFormat::Cargo,
                &["cargo-secret", "cargo-proxy"],
            ),
        ],
        ..Default::default()
    })
    .await;
    publish(&a, "cargo-secret", CRATE, "1.0.0", &secret).await;

    let index = index_url(&a, "cargo-all", CRATE);
    let dl = download_url(&a, "cargo-all", CRATE, "1.0.0");
    for url in [&index, &dl] {
        let resp = get(url).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "GET {url}: a private member is invisible, not forbidden"
        );
    }
    assert_eq!(
        up.tap.count(&up.index_path(CRATE)),
        1,
        "the public proxy member was asked once, then its 404 was served from the negative entry"
    );

    let entries = index_entries(get_with_token(&index).await).await;
    assert_eq!(entries, vec![("1.0.0".to_string(), cksum(&secret))]);
    let resp = get_with_token(&dl).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), secret.as_slice());
}
