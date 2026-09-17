mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::upstream_tap::{self, Tap};
use common::{
    build_npm_publish_body, build_tarball, expire_entries, group, hosted, proxy, spawn_server,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const PKG: &str = "@acme/widget";
const VERSION: &str = "1.0.0";
const TARBALL: &str = "widget-1.0.0.tgz";
const UPSTREAM_REPO: &str = "npm-hosted";

/// A second opencargo holding one published package, fronted by a tap.
struct Upstream {
    server: TestServer,
    tap: Tap,
    tarball: Vec<u8>,
}

impl Upstream {
    fn packument_path(&self) -> String {
        format!("/{UPSTREAM_REPO}/{PKG}")
    }

    fn tarball_path(&self) -> String {
        format!("/{UPSTREAM_REPO}/{PKG}/-/{TARBALL}")
    }

    fn url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}", self.tap.base_url)
    }
}

/// Publish one version of `name` into `repo` and return its tarball bytes.
async fn publish(server: &TestServer, repo: &str, name: &str, description: &str) -> Vec<u8> {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"{VERSION}"}}"#));
    let body = build_npm_publish_body(name, VERSION, description, &tarball);
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{name}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish {name} to {repo} should succeed");
    tarball
}

async fn seed_upstream() -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(UPSTREAM_REPO, RepositoryFormat::Npm, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let tarball = publish(&server, UPSTREAM_REPO, PKG, "a widget").await;
    let tap = upstream_tap::start(&server.base_url).await;
    Upstream {
        server,
        tap,
        tarball,
    }
}

async fn spawn_proxy(up: &Upstream) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![proxy("npm-proxy", RepositoryFormat::Npm, &up.url())],
        ..Default::default()
    })
    .await
}

/// `npm-group` = hosted `npm-local` first, then `npm-proxy` in front of `up`.
async fn spawn_group(up: &Upstream) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-local", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, &up.url()),
            group("npm-group", RepositoryFormat::Npm, &["npm-local", "npm-proxy"]),
        ],
        ..Default::default()
    })
    .await
}

async fn get_json(url: &str) -> Value {
    let resp = reqwest::get(url).await.expect("request failed");
    assert_eq!(resp.status(), StatusCode::OK, "GET {url}");
    resp.json().await.expect("invalid json")
}

async fn get_bytes(url: &str) -> Vec<u8> {
    let resp = reqwest::get(url).await.expect("request failed");
    assert_eq!(resp.status(), StatusCode::OK, "GET {url}");
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

fn tarball_url(packument: &Value) -> &str {
    packument["versions"][VERSION]["dist"]["tarball"]
        .as_str()
        .expect("version should carry dist.tarball")
}

#[tokio::test]
async fn proxy_serves_packument_and_tarball_from_second_instance() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;

    let packument = get_json(&format!("{}/npm-proxy/{PKG}", a.base_url)).await;
    assert_eq!(packument["name"], PKG);
    assert_eq!(packument["dist-tags"]["latest"], VERSION);
    assert!(packument["versions"][VERSION].is_object());

    let bytes = get_bytes(tarball_url(&packument)).await;
    assert_eq!(bytes, up.tarball, "tarball bytes should be the published ones");

    assert_eq!(up.tap.count(&up.packument_path()), 1);
    assert_eq!(up.tap.count(&up.tarball_path()), 1);
}

#[tokio::test]
async fn tarball_urls_point_at_proxy() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;

    let packument = get_json(&format!("{}/npm-proxy/{PKG}", a.base_url)).await;
    let versions = packument["versions"]
        .as_object()
        .expect("versions should be an object");
    assert!(!versions.is_empty());
    let prefix = format!("{}/npm-proxy/{PKG}/-/", a.base_url);
    for (version, meta) in versions {
        let url = meta["dist"]["tarball"].as_str().expect("dist.tarball");
        assert!(url.starts_with(&prefix), "{version}: {url} should start with {prefix}");
        assert!(!url.contains(&up.tap.base_url), "{version}: {url} leaks the tap");
        assert!(!url.contains(&up.server.base_url), "{version}: {url} leaks the upstream");
    }
}

#[tokio::test]
async fn tarball_immutable_one_upstream_hit() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/npm-proxy/{PKG}/-/{TARBALL}", a.base_url);

    let first = get_bytes(&url).await;
    let second = get_bytes(&url).await;
    expire_entries(&a).await;
    let third = get_bytes(&url).await;

    assert_eq!(first, up.tarball);
    assert_eq!(second, up.tarball);
    assert_eq!(third, up.tarball);
    assert_eq!(
        up.tap.count(&up.tarball_path()),
        1,
        "an immutable tarball is fetched upstream exactly once"
    );
}

#[tokio::test]
async fn group_falls_through_to_proxy() {
    let up = seed_upstream().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-local", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, &up.url()),
            group("npm-group", RepositoryFormat::Npm, &["npm-local", "npm-proxy"]),
        ],
        ..Default::default()
    })
    .await;

    let packument = get_json(&format!("{}/npm-group/{PKG}", a.base_url)).await;
    assert_eq!(packument["name"], PKG);
    assert!(packument["versions"][VERSION].is_object());

    let bytes = get_bytes(&format!("{}/npm-group/{PKG}/-/{TARBALL}", a.base_url)).await;
    assert_eq!(bytes, up.tarball);

    assert_eq!(up.tap.count(&up.packument_path()), 1);
    assert_eq!(up.tap.count(&up.tarball_path()), 1);
}

#[tokio::test]
async fn missing_package_negative_cached_one_hit() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/npm-proxy/@acme/nope", a.base_url);

    for _ in 0..2 {
        let resp = reqwest::get(&url).await.expect("request failed");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    assert_eq!(
        up.tap.count(&format!("/{UPSTREAM_REPO}/@acme/nope")),
        1,
        "the second miss is answered from the negative entry"
    );
    let rows = cache_rows(&a).await;
    assert_eq!(
        rows,
        vec![("npm-metadata".to_string(), "@acme/nope".to_string(), 404)]
    );
}

#[tokio::test]
async fn packument_ttl_expiry_refetches() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/npm-proxy/{PKG}", a.base_url);

    let first = get_json(&url).await;
    get_json(&url).await;
    assert_eq!(up.tap.count(&up.packument_path()), 1, "a fresh row never hits upstream");

    expire_entries(&a).await;
    let refreshed = get_json(&url).await;
    assert_eq!(up.tap.count(&up.packument_path()), 2, "an expired row is refetched once");
    assert_eq!(refreshed, first);
    let rows = cache_rows(&a).await;
    assert_eq!(rows, vec![("npm-metadata".to_string(), PKG.to_string(), 200)]);
}

#[tokio::test]
async fn upstream_503_serves_stale_metadata() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/npm-proxy/{PKG}", a.base_url);

    let fresh = get_json(&url).await;
    expire_entries(&a).await;
    up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);

    let resp = reqwest::get(&url).await.expect("request failed");
    assert_eq!(resp.status(), StatusCode::OK, "a stale packument beats a 503");
    let warning = resp
        .headers()
        .get("warning")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(warning.starts_with("110"), "stale response must carry Warning 110, got {warning:?}");
    let stale: Value = resp.json().await.expect("invalid json");
    assert_eq!(stale, fresh);
    assert_eq!(up.tap.count(&up.packument_path()), 2, "the refresh was attempted once");
}

#[tokio::test]
async fn group_upstream_failure_is_502_not_404() {
    let up = seed_upstream().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-local", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, &up.url()),
            group("npm-group", RepositoryFormat::Npm, &["npm-local", "npm-proxy"]),
        ],
        ..Default::default()
    })
    .await;
    up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);

    for url in [
        format!("{}/npm-group/{PKG}", a.base_url),
        format!("{}/npm-group/{PKG}/-/{TARBALL}", a.base_url),
        format!("{}/npm-proxy/{PKG}", a.base_url),
    ] {
        let resp = reqwest::get(&url).await.expect("request failed");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "GET {url}: an upstream 503 with no cached copy is a 502"
        );
    }

    assert_eq!(
        up.tap.count(&up.packument_path()),
        2,
        "every miss reached the upstream"
    );
    assert_eq!(up.tap.count(&up.tarball_path()), 1);
    assert!(cache_rows(&a).await.is_empty(), "failures are never cached");
}

#[tokio::test]
async fn group_dist_tags_via_proxy_member() {
    let up = seed_upstream().await;
    let a = spawn_group(&up).await;
    let client = reqwest::Client::new();

    for repo in ["npm-group", "npm-proxy"] {
        let url = format!("{}/{repo}/-/package/{PKG}/dist-tags", a.base_url);
        assert_eq!(get_json(&url).await, json!({ "latest": VERSION }), "{repo}");
        let resp = client
            .put(format!("{url}/beta"))
            .bearer_auth(STATIC_TOKEN)
            .json(&json!(VERSION))
            .send()
            .await
            .expect("put dist-tag failed");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{repo}: dist-tag writes are hosted-only");
    }
    assert_eq!(
        up.tap.count(&up.packument_path()),
        1,
        "dist-tags are read from the one cached packument"
    );

    let resp = client
        .get(format!("{}/npm-group/-/package/@acme/nope/dist-tags", a.base_url))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_recurses_nested_groups() {
    let up = seed_upstream().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-local", RepositoryFormat::Npm, Visibility::Public),
            hosted("npm-other", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, &up.url()),
            group("npm-inner", RepositoryFormat::Npm, &["npm-local", "npm-proxy"]),
            group("npm-outer", RepositoryFormat::Npm, &["npm-inner", "npm-other"]),
        ],
        ..Default::default()
    })
    .await;
    publish(&a, "npm-local", "@acme/gizmo", "zzsearchword from local").await;
    publish(&a, "npm-other", "@acme/gizmo", "zzsearchword from other").await;
    publish(&a, "npm-other", "@acme/gadget", "zzsearchword too").await;

    let url = format!("{}/npm-outer/-/v1/search?text=zzsearchword", a.base_url);
    let result = get_json(&url).await;
    let objects = result["objects"].as_array().expect("objects");
    let names: Vec<&str> = objects
        .iter()
        .map(|o| o["package"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["@acme/gizmo", "@acme/gadget"], "nested first, deduplicated");
    assert_eq!(objects[0]["package"]["description"], "zzsearchword from local");
    assert_eq!(result["total"], 2);

    let page = get_json(&format!("{url}&from=1&size=1")).await;
    assert_eq!(page["objects"][0]["package"]["name"], "@acme/gadget");
    assert_eq!(page["total"], 2);
    assert!(up.tap.hits.lock().unwrap().is_empty(), "search never asks the upstream");
}
