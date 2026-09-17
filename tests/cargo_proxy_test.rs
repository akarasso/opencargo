mod common;

use std::io::Write;

use reqwest::StatusCode;
use serde_json::Value;
use sha2::Digest;

use common::fake_upstream::cargo::{self as fake_index, FakeIndex};
use common::upstream_tap::{self, Tap};
use common::{
    build_cargo_publish_body, expire_entries, group, hosted, proxy, proxy_with, spawn_server,
    ProxyOpts, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};
use opencargo::proxy::UpstreamAuth;
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

/// cargo lowercases the index path but keeps the crate's case in `dl`; the
/// hosted lookup follows suit, and the line carries the published case.
#[tokio::test]
async fn hosted_mixed_case_crate_resolves_through_lowercase_index_path() {
    let a = spawn_server(SpawnOpts {
        repositories: vec![hosted("cargo-local", RepositoryFormat::Cargo, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let bytes = crate_bytes("MyCrate");
    let meta = r#"{"name":"MyCrate","vers":"1.0.0","deps":[{"name":"serde","version_req":"^1","kind":"normal","features":[],"optional":false,"default_features":true,"target":null,"registry":null,"explicit_name_in_toml":null}],"features":{},"authors":[],"description":"d"}"#;
    let resp = reqwest::Client::new()
        .put(format!("{}/cargo-local/api/v1/crates/new", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(build_cargo_publish_body(meta, &bytes))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = get(&format!("{}/cargo-local/index/my/cr/mycrate", a.base_url)).await;
    assert_eq!(resp.status(), StatusCode::OK, "the lowercase path cargo requests");
    let line: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    assert_eq!(line["name"], "MyCrate", "the published case, as crates.io does");
    assert_eq!(line["deps"][0]["req"], "^1");
    assert!(line["deps"][0].get("version_req").is_none());

    for name in ["MyCrate", "mycrate"] {
        assert_eq!(download(&download_url(&a, "cargo-local", name, "1.0.0")).await, bytes);
    }
    let resp = reqwest::Client::new()
        .put(format!("{}/cargo-local/api/v1/crates/new", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(build_cargo_publish_body(&publish_meta("mycrate", "1.0.0"), &bytes))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "another casing is the same crate"
    );
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

/// Sum of the upstream's download counters: one per served `.crate`.
async fn upstream_downloads(up: &Upstream) -> i64 {
    let db_path = up.server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the upstream database");
    let (count,): (i64,) = sqlx::query_as("SELECT COALESCE(SUM(count), 0) FROM download_counts")
        .fetch_one(&pool)
        .await
        .expect("failed to read download counts");
    pool.close().await;
    count
}

async fn spawn_proxy_over_fake(fake: &FakeIndex, opts: ProxyOpts) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![proxy_with(
            "cargo-proxy",
            RepositoryFormat::Cargo,
            &fake.index_url(),
            opts,
        )],
        ..Default::default()
    })
    .await
}

/// Every regular file below `dir`, recursively; an absent dir holds none.
fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|e| {
            if e.path().is_dir() {
                files_under(&e.path())
            } else {
                vec![e.path()]
            }
        })
        .collect()
}

fn allow_private() -> ProxyOpts {
    ProxyOpts {
        dl_allow_private: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn proxy_index_and_download_from_second_instance() {
    let up = spawn_upstream().await;
    let bytes = crate_bytes("widget-1.0.0");
    publish(&up.server, UPSTREAM_REPO, CRATE, "1.0.0", &bytes).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;

    let resp = get(&index_url(&a, "cargo-proxy", CRATE)).await;
    assert!(
        resp.headers().contains_key("etag"),
        "the index carries an ETag"
    );
    let entries = index_entries(resp).await;
    assert_eq!(entries, vec![("1.0.0".to_string(), cksum(&bytes))]);

    let dl = download_url(&a, "cargo-proxy", CRATE, "1.0.0");
    assert_eq!(download(&dl).await, bytes);
    assert_eq!(download(&dl).await, bytes);
    assert_eq!(
        up.tap.count(&up.index_path(CRATE)),
        1,
        "a fresh index row never hits upstream"
    );
    assert_eq!(
        upstream_downloads(&up).await,
        1,
        "an immutable crate is downloaded once"
    );

    let mut rows = cache_rows(&a).await;
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("cargo-config".to_string(), "config.json".to_string(), 200),
            ("cargo-crate".to_string(), "widget/1.0.0".to_string(), 200),
            ("cargo-index".to_string(), "widget".to_string(), 200),
        ]
    );
}

#[tokio::test]
async fn download_verifies_cksum_from_index() {
    let fake = fake_index::start().await;
    let bytes = crate_bytes("tampered");
    fake.add_crate_with_cksum(CRATE, "1.0.0", &bytes, &"00".repeat(32));
    let a = spawn_proxy_over_fake(&fake, allow_private()).await;

    let resp = get(&download_url(&a, "cargo-proxy", CRATE, "1.0.0")).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "a cksum mismatch is 502"
    );
    assert_eq!(
        fake.downloads().len(),
        1,
        "the body was fetched, then refused"
    );
    let rows = cache_rows(&a).await;
    assert!(
        !rows.iter().any(|(kind, _, _)| kind == "cargo-crate"),
        "nothing stored for the crate: {rows:?}"
    );
    let crate_dir = a
        .tmp
        .path()
        .join("storage/_proxy_cache/cargo-proxy/cargo-crate");
    assert!(
        files_under(&crate_dir).is_empty(),
        "no file, not even a part, is left behind: {:?}",
        files_under(&crate_dir)
    );
}

#[tokio::test]
async fn dl_template_markers_and_default_expand() {
    let fake = fake_index::start().await;
    let plain = crate_bytes("plain");
    let marked = crate_bytes("marked");
    let plain_cksum = fake.add_crate(CRATE, "1.0.0", &plain);
    let marked_cksum = fake.add_crate(CRATE, "1.1.0", &marked);
    let a = spawn_proxy_over_fake(&fake, allow_private()).await;

    assert_eq!(
        download(&download_url(&a, "cargo-proxy", CRATE, "1.0.0")).await,
        plain
    );
    assert_eq!(
        fake.downloads(),
        vec!["/dl/widget/1.0.0/download".to_string()],
        "a dl without markers gets /{{crate}}/{{version}}/download appended"
    );

    fake.set_dl("/{lowerprefix}/{prefix}/{crate}/{crate}-{version}.crate?cksum={sha256-checksum}");
    expire_entries(&a).await;
    assert_eq!(
        download(&download_url(&a, "cargo-proxy", CRATE, "1.1.0")).await,
        marked
    );
    assert_eq!(
        fake.downloads().last().map(String::as_str),
        Some(format!("/dl/wi/dg/wi/dg/widget/widget-1.1.0.crate?cksum={marked_cksum}").as_str()),
        "every marker is substituted"
    );
    assert_ne!(plain_cksum, marked_cksum);
}

#[tokio::test]
async fn index_etag_revalidation_touches_row() {
    let fake = fake_index::start().await;
    let bytes = crate_bytes("etag");
    fake.add_crate(CRATE, "1.0.0", &bytes);
    let a = spawn_proxy_over_fake(&fake, allow_private()).await;
    let url = index_url(&a, "cargo-proxy", CRATE);
    let path = fake.index_path(CRATE);

    let first = get(&url).await;
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let entries = index_entries(first).await;
    get(&url).await;
    assert_eq!(fake.count(&path), 1, "a fresh row never hits upstream");

    expire_entries(&a).await;
    let refreshed = index_entries(get(&url).await).await;
    assert_eq!(refreshed, entries);
    assert_eq!(fake.count(&path), 2, "an expired row is revalidated once");
    assert_eq!(
        fake.revalidations(&path),
        1,
        "with If-None-Match, answered 304"
    );
    get(&url).await;
    assert_eq!(fake.count(&path), 2, "the 304 renewed the row's TTL");

    let resp = reqwest::Client::new()
        .get(&url)
        .header("if-none-match", &etag)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_MODIFIED,
        "clients revalidate against us too"
    );
}

#[tokio::test]
async fn prefix_routes_for_1_2_3_4_char_names() {
    let up = spawn_upstream().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;
    for (name, prefix) in [("a", "1"), ("ab", "2"), ("abc", "3/a"), ("abcd", "ab/cd")] {
        publish(&up.server, UPSTREAM_REPO, name, "0.1.0", &crate_bytes(name)).await;
        let url = format!("{}/cargo-proxy/index/{prefix}/{name}", a.base_url);
        let entries = index_entries(get(&url).await).await;
        assert_eq!(entries.len(), 1, "{name} under {prefix}");
        assert_eq!(
            up.tap
                .count(&format!("/{UPSTREAM_REPO}/index/{prefix}/{name}")),
            1
        );
    }
    let resp = get(&format!("{}/cargo-proxy/index/2/a", a.base_url)).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a prefix cargo would not derive is 404"
    );
    assert_eq!(
        up.tap.count(&format!("/{UPSTREAM_REPO}/index/2/a")),
        0,
        "and never asked upstream"
    );
}

#[tokio::test]
async fn config_json_anonymous_on_private_repo_points_at_requested_repo_with_auth_required() {
    let up = spawn_upstream().await;
    let a = spawn_server(SpawnOpts {
        anonymous_read: false,
        repositories: vec![RepositoryConfig {
            visibility: Visibility::Private,
            ..proxy_repo("cargo-proxy", &up)
        }],
        ..Default::default()
    })
    .await;

    let resp = get(&format!("{}/cargo-proxy/index/config.json", a.base_url)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "no credentials sent, still 200"
    );
    let config: Value = resp.json().await.expect("invalid json");
    assert_eq!(
        config["dl"],
        format!("{}/cargo-proxy/api/v1/crates", a.base_url)
    );
    assert_eq!(config["api"], format!("{}/cargo-proxy", a.base_url));
    assert_eq!(config["auth-required"], true);
    assert_eq!(
        up.tap.hits.lock().unwrap().len(),
        0,
        "config.json is generated, never proxied"
    );

    let resp = get(&index_url(&a, "cargo-proxy", CRATE)).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the index stays gated"
    );
}

#[tokio::test]
async fn unknown_crate_negative_cached() {
    let up = spawn_upstream().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;

    for url in [
        index_url(&a, "cargo-proxy", "nope"),
        index_url(&a, "cargo-proxy", "nope"),
        download_url(&a, "cargo-proxy", "nope", "1.0.0"),
    ] {
        let resp = get(&url).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "GET {url}");
    }
    assert_eq!(
        up.tap.count(&up.index_path("nope")),
        1,
        "the second and third misses are answered from the negative entry"
    );
    let rows = cache_rows(&a).await;
    assert!(
        rows.contains(&("cargo-index".to_string(), "nope".to_string(), 404)),
        "{rows:?}"
    );
}

#[tokio::test]
async fn known_crate_unknown_version_is_404() {
    let up = spawn_upstream().await;
    publish(&up.server, UPSTREAM_REPO, CRATE, "1.0.0", &crate_bytes("w")).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;

    let resp = get(&download_url(&a, "cargo-proxy", CRATE, "9.9.9")).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the index knows the crate but not the version"
    );
    assert_eq!(
        upstream_downloads(&up).await,
        0,
        "no download was attempted"
    );
    let rows = cache_rows(&a).await;
    assert!(
        rows.iter()
            .any(|(k, key, s)| k == "cargo-index" && key == CRATE && *s == 200),
        "the index itself was read and cached: {rows:?}"
    );
    assert!(
        !rows.iter().any(|(k, _, _)| k == "cargo-crate"),
        "no crate row: {rows:?}"
    );
}

#[tokio::test]
async fn upstream_503_is_502() {
    let up = spawn_upstream().await;
    publish(&up.server, UPSTREAM_REPO, CRATE, "1.0.0", &crate_bytes("w")).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;
    up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);

    for url in [
        index_url(&a, "cargo-proxy", CRATE),
        download_url(&a, "cargo-proxy", CRATE, "1.0.0"),
    ] {
        let resp = get(&url).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "GET {url}: an upstream 503 with no cached copy is a 502, never a 404"
        );
    }
    assert!(cache_rows(&a).await.is_empty(), "failures are never cached");
}

#[tokio::test]
async fn stale_index_served_on_upstream_error() {
    let up = spawn_upstream().await;
    publish(&up.server, UPSTREAM_REPO, CRATE, "1.0.0", &crate_bytes("w")).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy_repo("cargo-proxy", &up)],
        ..Default::default()
    })
    .await;
    let url = index_url(&a, "cargo-proxy", CRATE);

    let fresh = index_entries(get(&url).await).await;
    expire_entries(&a).await;
    up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);

    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::OK, "a stale index beats a 503");
    let warning = resp
        .headers()
        .get("warning")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        warning.starts_with("110"),
        "stale response carries Warning 110, got {warning:?}"
    );
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(index_entries(resp).await, fresh);
    assert_eq!(
        up.tap.count(&up.index_path(CRATE)),
        2,
        "the refresh was attempted once"
    );

    let resp = reqwest::Client::new()
        .get(&url)
        .header("if-none-match", &etag)
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let warning = resp
        .headers()
        .get("warning")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        warning.starts_with("110"),
        "a 304 on a stale index still says so, got {warning:?}"
    );
}

#[tokio::test]
async fn index_routes_leave_npm_and_go_packages_named_index_alone() {
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            hosted("go-hosted", RepositoryFormat::Go, Visibility::Public),
        ],
        ..Default::default()
    })
    .await;
    for (path, handler) in [
        ("/npm-hosted/index/-/index-1.0.0.tgz", "npm tarball"),
        ("/go-hosted/index/@latest", "go @latest"),
    ] {
        let resp = get(&format!("{}{path}", a.base_url)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{handler}");
        let body = resp.text().await.expect("failed to read body");
        assert!(
            !body.contains("cargo"),
            "{handler} answered by the cargo index handler: {body}"
        );
    }
}

#[tokio::test]
async fn dl_host_on_private_literal_is_refused_without_optin() {
    let fake = fake_index::start().await;
    fake.add_crate(CRATE, "1.0.0", &crate_bytes("w"));
    fake.set_dl_absolute("http://127.0.0.1:1/");
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy(
            "cargo-proxy",
            RepositoryFormat::Cargo,
            &fake.index_url(),
        )],
        ..Default::default()
    })
    .await;

    let resp = get(&download_url(&a, "cargo-proxy", CRATE, "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let rows = cache_rows(&a).await;
    assert!(
        !rows.iter().any(|(kind, _, _)| kind == "cargo-crate"),
        "no crate row: {rows:?}"
    );
    assert!(
        fake.downloads().is_empty(),
        "no download was attempted anywhere"
    );
    assert_eq!(
        fake.count(&fake.index_path(CRATE)),
        1,
        "the index itself was read"
    );
}

/// The guard resolves names: `localhost` is as private as `127.0.0.1`.
#[tokio::test]
async fn dl_host_resolving_to_private_address_is_refused_without_optin() {
    let fake = fake_index::start().await;
    let bytes = crate_bytes("w");
    fake.add_crate(CRATE, "1.0.0", &bytes);
    let port = fake.base_url.rsplit(':').next().unwrap();
    fake.set_dl_absolute(&format!("http://localhost:{port}/dl"));
    let strict = spawn_proxy_over_fake(&fake, ProxyOpts::default()).await;

    let resp = get(&download_url(&strict, "cargo-proxy", CRATE, "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(resp.text().await.unwrap().contains("private address"));
    assert!(fake.downloads().is_empty(), "never contacted through its name");

    let lenient = spawn_proxy_over_fake(&fake, allow_private()).await;
    assert_eq!(
        download(&download_url(&lenient, "cargo-proxy", CRATE, "1.0.0")).await,
        bytes
    );
}

#[tokio::test]
async fn dl_off_host_never_sees_upstream_credentials() {
    let index = fake_index::start().await;
    let elsewhere = fake_index::start().await;
    let bytes = crate_bytes("guarded");
    index.add_crate(CRATE, "1.0.0", &bytes);
    elsewhere.add_crate(CRATE, "1.0.0", &bytes);
    index.set_dl_absolute(&format!("{}/dl", elsewhere.base_url));
    let a = spawn_proxy_over_fake(
        &index,
        ProxyOpts {
            upstream_auth: Some(UpstreamAuth::Bearer {
                token: "secret".into(),
            }),
            ..allow_private()
        },
    )
    .await;

    let url = download_url(&a, "cargo-proxy", CRATE, "1.0.0");
    let resp = get(&url).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "a dl off the index host is refused while upstream_auth is set"
    );
    assert!(
        elsewhere.downloads().is_empty(),
        "the other host was never contacted, so it never saw the credentials"
    );
    assert_eq!(
        index.authorization(&index.index_path(CRATE)).as_deref(),
        Some("Bearer secret"),
        "the index itself is read with the credentials"
    );

    index.set_dl("");
    expire_entries(&a).await;
    assert_eq!(download(&url).await, bytes);
    assert_eq!(
        index
            .authorization(&format!("/dl/{CRATE}/1.0.0/download"))
            .as_deref(),
        Some("Bearer secret"),
        "a same-origin dl keeps the credentials"
    );
}
