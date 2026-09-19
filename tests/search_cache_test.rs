//! Search over what the proxy has served (docs/design/search-over-cache.md).
//!
//! The gap these pin: a package fetched through a proxy member had no row in
//! `packages`, so both search surfaces -- `npm search` and the admin panel --
//! answered "nothing" for a package the server had served minutes earlier.

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::fake_upstream::cargo as fake_cargo;
use common::fake_upstream::go as fake_go;
use common::fake_upstream::npm as fake_npm;
use common::{
    build_npm_publish_body, build_tarball, expire_entries, group, hosted, proxy, spawn_server,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryConfig, RepositoryFormat, RepositoryType, Visibility};
use opencargo::registry::cargo::compute_prefix;

const PKG: &str = "left-pad";
const PROXY: &str = "npm-proxy";
const LOCAL: &str = "npm-local";
const GROUP: &str = "npm-group";

fn packument(latest: &str, description: &str) -> Value {
    json!({
        "name": PKG,
        "description": description,
        "dist-tags": { "latest": latest },
        "versions": { latest: { "name": PKG, "version": latest, "dist": {
            "tarball": format!("http://upstream.invalid/{PKG}/-/{PKG}-{latest}.tgz")
        } } },
    })
}

fn private_proxy(name: &str, fmt: RepositoryFormat, upstream: &str) -> RepositoryConfig {
    RepositoryConfig {
        name: name.to_string(),
        repo_type: RepositoryType::Proxy,
        format: fmt,
        visibility: Visibility::Private,
        upstream: Some(upstream.to_string()),
        ..Default::default()
    }
}

async fn get(srv: &TestServer, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = reqwest::Client::new().get(format!("{}{path}", srv.base_url));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let resp = request.send().await.expect("request failed");
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One fetch through a proxy member, which is what puts the package in the
/// index; the body itself is asserted by the proxy suites.
async fn fetch(srv: &TestServer, repo: &str, package: &str) {
    let (status, _) = get(srv, &format!("/{repo}/{package}"), Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "GET /{repo}/{package}");
}

async fn panel(srv: &TestServer, query: &str, token: Option<&str>) -> Vec<Value> {
    let (status, body) = get(srv, &format!("/api/v1/search?q={query}"), token).await;
    assert_eq!(status, StatusCode::OK, "search ?q={query}");
    body["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results is not an array: {body}"))
        .clone()
}

async fn npm_search(srv: &TestServer, repo: &str, text: &str) -> Vec<Value> {
    let (status, body) = get(
        srv,
        &format!("/{repo}/-/v1/search?text={text}"),
        Some(STATIC_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "npm search on {repo}");
    body["objects"]
        .as_array()
        .unwrap_or_else(|| panic!("objects is not an array: {body}"))
        .clone()
}

async fn publish(srv: &TestServer, repo: &str, package: &str, version: &str, description: &str) {
    let manifest = format!(r#"{{"name":"{package}","version":"{version}"}}"#);
    let body = build_npm_publish_body(package, version, description, &build_tarball(&manifest));
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{package}", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The whole point: one fetch, and the package is in the panel, named as
/// proxied and carrying the repository a client has to install it from.
#[tokio::test]
async fn a_proxied_package_is_in_the_panel_after_one_fetch() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy(PROXY, RepositoryFormat::Npm, &fake.base_url)],
        ..Default::default()
    })
    .await;

    assert!(
        panel(&srv, PKG, Some(STATIC_TOKEN)).await.is_empty(),
        "a package this server has never fetched is not in the index"
    );

    fetch(&srv, PROXY, PKG).await;

    let results = panel(&srv, PKG, Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["name"], json!(PKG));
    assert_eq!(results[0]["source"], json!("cached"));
    assert_eq!(results[0]["repository"], json!(PROXY));
    assert_eq!(results[0]["latest_version"], json!("1.0.0"));
    assert!(
        results[0]["last_seen"].is_string(),
        "a cached row says when it was last served: {results:?}"
    );
}

/// The description the packument carries is searched too, and the search
/// answers from the index rather than from the upstream.
#[tokio::test]
async fn a_word_of_the_description_finds_it_without_asking_the_upstream() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy(PROXY, RepositoryFormat::Npm, &fake.base_url)],
        ..Default::default()
    })
    .await;
    fetch(&srv, PROXY, PKG).await;
    let after_fetch = fake.hits().len();

    let results = panel(&srv, "zzpadword", Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["name"], json!(PKG));

    assert_eq!(
        fake.hits().len(),
        after_fetch,
        "a search never leaves the process"
    );
}

/// `npm search` on the member and on a group above it, which is the surface
/// a client actually calls.
#[tokio::test]
async fn npm_search_finds_it_on_the_member_and_on_the_group_above() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![
            hosted(LOCAL, RepositoryFormat::Npm, Visibility::Public),
            proxy(PROXY, RepositoryFormat::Npm, &fake.base_url),
            group(GROUP, RepositoryFormat::Npm, &[LOCAL, PROXY]),
        ],
        ..Default::default()
    })
    .await;
    fetch(&srv, PROXY, PKG).await;
    let after_fetch = fake.hits().len();

    for repo in [PROXY, GROUP] {
        let objects = npm_search(&srv, repo, "zzpadword").await;
        assert_eq!(objects.len(), 1, "{repo}: {objects:?}");
        assert_eq!(objects[0]["package"]["name"], json!(PKG));
        assert_eq!(objects[0]["package"]["version"], json!("1.0.0"));
    }

    assert_eq!(
        fake.hits().len(),
        after_fetch,
        "search never asks the upstream"
    );
}

/// A hosted package wins the name a proxy also serves: one answer, and it
/// names where the client should get it.
#[tokio::test]
async fn a_hosted_package_collapses_the_cached_row_of_the_same_name() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![
            hosted(LOCAL, RepositoryFormat::Npm, Visibility::Public),
            proxy(PROXY, RepositoryFormat::Npm, &fake.base_url),
        ],
        ..Default::default()
    })
    .await;
    fetch(&srv, PROXY, PKG).await;
    publish(&srv, LOCAL, PKG, "9.9.9", "zzpadword ours").await;

    let results = panel(&srv, PKG, Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "one package, one answer: {results:?}");
    assert_eq!(results[0]["source"], json!("hosted"));
    assert_eq!(results[0]["repository"], json!(LOCAL));
    assert_eq!(results[0]["latest_version"], json!("9.9.9"));
}

/// A cached row is visible exactly to the callers its repository is visible
/// to: the scope is the same predicate as the hosted index's.
#[tokio::test]
async fn a_private_proxys_rows_are_the_admins_to_find() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![private_proxy(
            "npm-private",
            RepositoryFormat::Npm,
            &fake.base_url,
        )],
        ..Default::default()
    })
    .await;
    fetch(&srv, "npm-private", PKG).await;

    assert!(
        panel(&srv, PKG, None).await.is_empty(),
        "an anonymous caller must not find a private repository's package"
    );
    let admin = panel(&srv, PKG, Some(STATIC_TOKEN)).await;
    assert_eq!(admin.len(), 1, "{admin:?}");
    assert_eq!(admin[0]["repository"], json!("npm-private"));
}

/// A second sighting is the same row: the index does not grow with every
/// fetch. That the row is refreshed rather than replaced is the port's own
/// clause, asserted on both adapters with an explicit clock.
#[tokio::test]
async fn a_second_fetch_leaves_one_row_rather_than_adding_another() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy(PROXY, RepositoryFormat::Npm, &fake.base_url)],
        ..Default::default()
    })
    .await;
    fetch(&srv, PROXY, PKG).await;
    expire_entries(&srv).await;
    fetch(&srv, PROXY, PKG).await;

    let results = panel(&srv, PKG, Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "one package, one row: {results:?}");
    assert_eq!(results[0]["latest_version"], json!("1.0.0"));
}

/// A purge is the operator saying the repository serves nothing; a refetch
/// puts the package back.
#[tokio::test]
async fn a_cache_purge_forgets_the_rows_and_a_refetch_brings_them_back() {
    let fake = fake_npm::start(packument("1.0.0", "zzpadword left padding"), "\"v1\"").await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy(PROXY, RepositoryFormat::Npm, &fake.base_url)],
        ..Default::default()
    })
    .await;
    fetch(&srv, PROXY, PKG).await;
    assert_eq!(panel(&srv, PKG, Some(STATIC_TOKEN)).await.len(), 1);

    let purged = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/repositories/{PROXY}/purge-cache",
            srv.base_url
        ))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("purge failed");
    assert_eq!(purged.status(), StatusCode::OK);

    assert!(
        panel(&srv, PKG, Some(STATIC_TOKEN)).await.is_empty(),
        "a purge takes the rows with the cache"
    );

    fetch(&srv, PROXY, PKG).await;
    assert_eq!(
        panel(&srv, PKG, Some(STATIC_TOKEN)).await.len(),
        1,
        "the package is findable again once it has been served again"
    );
}

/// Cargo: the index fetch is the sighting, and its newest line is the hint.
#[tokio::test]
async fn a_cargo_index_fetch_indexes_the_crate() {
    let fake = fake_cargo::start().await;
    fake.add_crate("zzserde", "1.0.0", b"a crate");
    fake.add_crate("zzserde", "1.1.0", b"a newer crate");
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy(
            "cargo-proxy",
            RepositoryFormat::Cargo,
            &fake.index_url(),
        )],
        ..Default::default()
    })
    .await;

    let (status, _) = get(
        &srv,
        &format!(
            "/cargo-proxy/index/{}/zzserde",
            compute_prefix("zzserde")
        ),
        Some(STATIC_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the index of a known crate");

    let results = panel(&srv, "zzserde", Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["source"], json!("cached"));
    assert_eq!(results[0]["repository"], json!("cargo-proxy"));
    assert_eq!(
        results[0]["latest_version"],
        json!("1.1.0"),
        "the newest line of the index is the hint"
    );
}

/// Go: the version list is the sighting, and the module path is the name.
#[tokio::test]
async fn a_go_list_fetch_indexes_the_module() {
    let fake = fake_go::start().await;
    let srv = spawn_server(SpawnOpts {
        repositories: vec![proxy("go-proxy", RepositoryFormat::Go, &fake.base_url)],
        ..Default::default()
    })
    .await;

    let (status, _) = get(
        &srv,
        &format!("/go-proxy/{}/@v/list", fake_go::MODULE),
        Some(STATIC_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the version list of a known module");

    let results = panel(&srv, "gone", Some(STATIC_TOKEN)).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["name"], json!(fake_go::MODULE));
    assert_eq!(results[0]["source"], json!("cached"));
}
