#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
//! SQLite-only by design: these assertions guarantee the schema, not the
//! ports (designs-next/ports-and-adapters.md 7.4).

mod common;

use std::io::Write;
use std::sync::atomic::Ordering;

use reqwest::StatusCode;
use serde_json::Value;

use common::fake_upstream::go as fake_go;
use common::upstream_tap::{self, Tap};
use common::{
    build_go_module_zip, expire_entries, group, hosted, proxy, publish_go_module, spawn_server,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const MODULE: &str = "example.com/org/lib";
const UPSTREAM_REPO: &str = "go-hosted";

/// A second opencargo holding published modules, fronted by a tap.
struct Upstream {
    server: TestServer,
    tap: Tap,
}

impl Upstream {
    fn url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}", self.tap.base_url)
    }

    fn path(&self, module: &str, suffix: &str) -> String {
        format!("/{UPSTREAM_REPO}/{module}/{suffix}")
    }

    async fn publish(&self, module: &str, version: &str) {
        publish_go_module(
            &reqwest::Client::new(),
            &self.server.base_url,
            UPSTREAM_REPO,
            module,
            version,
        )
        .await;
    }
}

async fn seed_upstream(versions: &[&str]) -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            UPSTREAM_REPO,
            RepositoryFormat::Go,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    let tap = upstream_tap::start(&server.base_url).await;
    let up = Upstream { server, tap };
    for version in versions {
        up.publish(MODULE, version).await;
    }
    up
}

async fn spawn_proxy(up: &Upstream) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![proxy("go-proxy", RepositoryFormat::Go, &up.url())],
        ..Default::default()
    })
    .await
}

async fn spawn_group(up: &Upstream, local: Visibility) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("go-local", RepositoryFormat::Go, local),
            proxy("go-proxy", RepositoryFormat::Go, &up.url()),
            group("go-group", RepositoryFormat::Go, &["go-local", "go-proxy"]),
        ],
        ..Default::default()
    })
    .await
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::get(url).await.expect("request failed")
}

async fn get_ok(url: &str) -> Vec<u8> {
    let resp = get(url).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {url}");
    resp.bytes().await.expect("failed to read body").to_vec()
}

async fn get_text(url: &str) -> String {
    String::from_utf8(get_ok(url).await).expect("body should be utf-8")
}

async fn get_json(url: &str) -> Value {
    serde_json::from_slice(&get_ok(url).await).expect("invalid json")
}

/// `(kind, cache_key, status, immutable)` of every cache row the server holds.
async fn cache_rows(server: &TestServer) -> Vec<(String, String, i64, bool)> {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    let rows = sqlx::query_as::<_, (String, String, i64, bool)>(
        "SELECT kind, cache_key, status, expires_at IS NULL FROM proxy_cache_entries ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read cache rows");
    pool.close().await;
    rows
}

/// A `packages` row without versions: a module the server knows but has
/// nothing for, which no publish can leave behind.
async fn insert_empty_package(server: &TestServer, repo: &str, module: &str) {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    sqlx::query(
        "INSERT INTO packages (repository_id, name) \
         SELECT id, ?2 FROM repositories WHERE name = ?1",
    )
    .bind(repo)
    .bind(module)
    .execute(&pool)
    .await
    .expect("failed to insert the package row");
    pool.close().await;
}

/// A module zip that differs from `build_go_module_zip` by one extra file.
fn build_marked_zip(module: &str, version: &str, marker: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        zip_writer
            .start_file(format!("{module}@{version}/go.mod"), options)
            .unwrap();
        zip_writer
            .write_all(format!("module {module}\n\ngo 1.21\n").as_bytes())
            .unwrap();
        zip_writer
            .start_file(format!("{module}@{version}/{marker}.go"), options)
            .unwrap();
        zip_writer.write_all(b"package lib\n").unwrap();
        zip_writer.finish().unwrap();
    }
    buf
}

async fn publish_zip(server: &TestServer, repo: &str, module: &str, version: &str, zip: Vec<u8>) {
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{module}/@v/{version}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", "application/zip")
        .body(zip)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "publish {module}@{version} to {repo}"
    );
}

#[tokio::test]
async fn proxy_list_info_mod_zip_bytes_exact() {
    let up = seed_upstream(&["v1.0.0"]).await;
    let a = spawn_proxy(&up).await;

    for suffix in [
        "@v/list",
        "@latest",
        "@v/v1.0.0.info",
        "@v/v1.0.0.mod",
        "@v/v1.0.0.zip",
    ] {
        let direct = get_ok(&format!(
            "{}/{UPSTREAM_REPO}/{MODULE}/{suffix}",
            up.server.base_url
        ))
        .await;
        let via_proxy = get_ok(&format!("{}/go-proxy/{MODULE}/{suffix}", a.base_url)).await;
        assert_eq!(
            via_proxy, direct,
            "{suffix}: proxied bytes must be the upstream's"
        );
        assert_eq!(up.tap.count(&up.path(MODULE, suffix)), 1, "{suffix}");
    }

    let zip = get_ok(&format!("{}/go-proxy/{MODULE}/@v/v1.0.0.zip", a.base_url)).await;
    assert_eq!(zip, build_go_module_zip(MODULE, "v1.0.0"));
    let info = get_json(&format!("{}/go-proxy/{MODULE}/@v/v1.0.0.info", a.base_url)).await;
    assert_eq!(info["Version"], "v1.0.0");
    assert!(chrono::DateTime::parse_from_rfc3339(info["Time"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn escaped_uppercase_module_roundtrip() {
    let raw = "example.com/Org/Lib";
    let escaped = "example.com/!org/!lib";
    let up = seed_upstream(&[]).await;
    up.publish(raw, "v1.0.0").await;
    let a = spawn_group(&up, Visibility::Public).await;
    publish_zip(
        &a,
        "go-local",
        raw,
        "v1.0.0",
        build_go_module_zip(raw, "v1.0.0"),
    )
    .await;

    for repo in ["go-proxy", "go-local", "go-group"] {
        let list = get_text(&format!("{}/{repo}/{escaped}/@v/list", a.base_url)).await;
        assert_eq!(list, "v1.0.0", "{repo}: the escaped module resolves");
        let zip = get_ok(&format!("{}/{repo}/{escaped}/@v/v1.0.0.zip", a.base_url)).await;
        assert_eq!(zip, build_go_module_zip(raw, "v1.0.0"), "{repo}");
    }
    assert_eq!(
        up.tap.count(&up.path(escaped, "@v/list")),
        1,
        "the escaped form travels upstream"
    );
    assert_eq!(
        up.tap.count(&up.path(raw, "@v/list")),
        0,
        "never the raw form"
    );

    let resp = get(&format!(
        "{}/go-proxy/example.com/!Org/lib/@v/list",
        a.base_url
    ))
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "`!` must be followed by a lowercase letter"
    );
}

#[tokio::test]
async fn non_canonical_version_is_ttl_not_immutable() {
    let up = seed_upstream(&["v1.0.0", "master"]).await;
    let a = spawn_proxy(&up).await;

    for version in ["v1.0.0", "master"] {
        let url = format!("{}/go-proxy/{MODULE}/@v/{version}.info", a.base_url);
        get_ok(&url).await;
        get_ok(&url).await;
    }
    let rows = cache_rows(&a).await;
    assert_eq!(
        rows,
        vec![
            ("go-info".into(), format!("{MODULE}/v1.0.0"), 200, true),
            ("go-info".into(), format!("{MODULE}/master"), 200, false),
        ]
    );

    expire_entries(&a).await;
    for version in ["v1.0.0", "master"] {
        get_ok(&format!(
            "{}/go-proxy/{MODULE}/@v/{version}.info",
            a.base_url
        ))
        .await;
    }
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/v1.0.0.info")),
        1,
        "canonical: immutable"
    );
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/master.info")),
        2,
        "query: refetched after expiry"
    );
}

#[tokio::test]
async fn list_and_latest_ttl_expiry() {
    let up = seed_upstream(&["v1.0.0"]).await;
    let a = spawn_proxy(&up).await;
    let list_url = format!("{}/go-proxy/{MODULE}/@v/list", a.base_url);
    let latest_url = format!("{}/go-proxy/{MODULE}/@latest", a.base_url);

    for _ in 0..2 {
        assert_eq!(get_text(&list_url).await, "v1.0.0");
        assert_eq!(get_json(&latest_url).await["Version"], "v1.0.0");
    }
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/list")),
        1,
        "a fresh row never hits upstream"
    );
    assert_eq!(up.tap.count(&up.path(MODULE, "@latest")), 1);

    up.publish(MODULE, "v1.1.0").await;
    expire_entries(&a).await;
    assert_eq!(get_text(&list_url).await, "v1.0.0\nv1.1.0");
    assert_eq!(get_json(&latest_url).await["Version"], "v1.1.0");
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/list")),
        2,
        "an expired row is refetched once"
    );
    assert_eq!(up.tap.count(&up.path(MODULE, "@latest")), 2);
    let rows = cache_rows(&a).await;
    assert_eq!(
        rows,
        vec![
            ("go-list".into(), MODULE.into(), 200, false),
            ("go-latest".into(), MODULE.into(), 200, false),
        ]
    );
}

#[tokio::test]
async fn group_list_union_and_latest_max_semver() {
    let up = seed_upstream(&["v1.0.0", "v1.2.0", "v1.9.0"]).await;
    let a = spawn_group(&up, Visibility::Public).await;
    let client = reqwest::Client::new();
    for version in ["v1.0.0", "v1.10.0"] {
        publish_go_module(&client, &a.base_url, "go-local", MODULE, version).await;
    }

    let list = get_text(&format!("{}/go-group/{MODULE}/@v/list", a.base_url)).await;
    let mut versions: Vec<&str> = list.lines().collect();
    assert_eq!(
        versions,
        ["v1.0.0", "v1.10.0", "v1.2.0", "v1.9.0"],
        "union in member order, dedup"
    );
    versions.sort_unstable();
    versions.dedup();
    assert_eq!(versions.len(), 4);

    let latest = get_json(&format!("{}/go-group/{MODULE}/@latest", a.base_url)).await;
    assert_eq!(
        latest["Version"], "v1.10.0",
        "semver max, not lexical (v1.9.0) nor last (v1.9.0)"
    );
    assert_eq!(up.tap.count(&up.path(MODULE, "@v/list")), 1);
    assert_eq!(up.tap.count(&up.path(MODULE, "@latest")), 1);
}

#[tokio::test]
async fn group_zip_first_member_wins() {
    let up = seed_upstream(&["v1.0.0", "v1.1.0"]).await;
    let a = spawn_group(&up, Visibility::Public).await;
    let local_zip = build_marked_zip(MODULE, "v1.0.0", "local");
    publish_zip(&a, "go-local", MODULE, "v1.0.0", local_zip.clone()).await;
    assert_ne!(local_zip, build_go_module_zip(MODULE, "v1.0.0"));

    let zip = get_ok(&format!("{}/go-group/{MODULE}/@v/v1.0.0.zip", a.base_url)).await;
    assert_eq!(zip, local_zip, "the hosted member comes first");
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/v1.0.0.zip")),
        0,
        "the proxy is never asked"
    );

    let zip = get_ok(&format!("{}/go-group/{MODULE}/@v/v1.1.0.zip", a.base_url)).await;
    assert_eq!(
        zip,
        build_go_module_zip(MODULE, "v1.1.0"),
        "a version only upstream falls through"
    );
    assert_eq!(up.tap.count(&up.path(MODULE, "@v/v1.1.0.zip")), 1);
    let mods = get_text(&format!("{}/go-group/{MODULE}/@v/v1.0.0.mod", a.base_url)).await;
    assert_eq!(mods, format!("module {MODULE}\n\ngo 1.21\n"));
}

#[tokio::test]
async fn unknown_module_list_is_404_known_empty_is_200() {
    let up = seed_upstream(&["v1.0.0"]).await;
    insert_empty_package(&up.server, UPSTREAM_REPO, "example.com/empty").await;
    let a = spawn_group(&up, Visibility::Public).await;

    for repo in ["go-proxy", "go-group"] {
        let resp = get(&format!("{}/{repo}/example.com/nope/@v/list", a.base_url)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{repo}: unknown module"
        );

        let resp = get(&format!("{}/{repo}/example.com/empty/@v/list", a.base_url)).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{repo}: known module with no versions"
        );
        assert_eq!(resp.text().await.unwrap(), "");
    }
    let resp = get(&format!(
        "{}/{UPSTREAM_REPO}/example.com/empty/@v/list",
        up.server.base_url
    ))
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "hosted: known module with no versions"
    );
}

#[tokio::test]
async fn group_hides_private_member() {
    let up = seed_upstream(&[]).await;
    up.publish("example.com/other", "v1.0.0").await;
    let a = spawn_group(&up, Visibility::Private).await;
    publish_go_module(
        &reqwest::Client::new(),
        &a.base_url,
        "go-local",
        MODULE,
        "v1.0.0",
    )
    .await;

    for suffix in ["@v/list", "@latest", "@v/v1.0.0.info", "@v/v1.0.0.zip"] {
        let url = format!("{}/go-group/{MODULE}/{suffix}", a.base_url);
        let resp = get(&url).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{suffix}: anonymous sees 404, never 401/403"
        );

        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .expect("request failed");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{suffix}: a reader of the member sees it"
        );
    }
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/list")),
        1,
        "the proxy member was consulted once"
    );
}

#[tokio::test]
async fn unknown_module_negative_cached_no_second_hit() {
    let up = seed_upstream(&["v1.0.0"]).await;
    let a = spawn_proxy(&up).await;

    for suffix in ["@v/list", "@latest", "@v/v9.9.9.info"] {
        let url = format!("{}/go-proxy/example.com/nope/{suffix}", a.base_url);
        for _ in 0..2 {
            assert_eq!(get(&url).await.status(), StatusCode::NOT_FOUND, "{suffix}");
        }
        assert_eq!(
            up.tap.count(&up.path("example.com/nope", suffix)),
            1,
            "{suffix}: one miss upstream"
        );
    }
    let rows = cache_rows(&a).await;
    assert_eq!(
        rows,
        vec![
            ("go-list".into(), "example.com/nope".into(), 404, false),
            ("go-latest".into(), "example.com/nope".into(), 404, false),
            (
                "go-info".into(),
                "example.com/nope/v9.9.9".into(),
                404,
                false
            ),
        ]
    );
}

#[tokio::test]
async fn upstream_410_is_404() {
    let fake = fake_go::start().await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy("go-proxy", RepositoryFormat::Go, &fake.base_url)],
        ..Default::default()
    })
    .await;
    let module = fake_go::MODULE;

    assert_eq!(
        get_text(&format!("{}/go-proxy/{module}/@v/list", a.base_url)).await,
        "v1.0.0"
    );
    for _ in 0..2 {
        let resp = get(&format!("{}/go-proxy/{module}/@v/v1.1.0.info", a.base_url)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "410 is a miss, so go moves on"
        );
    }
    assert_eq!(
        fake.count(&format!("/{module}/@v/v1.1.0.info")),
        1,
        "negative-cached"
    );
    let rows = cache_rows(&a).await;
    assert!(
        rows.contains(&("go-info".into(), format!("{module}/v1.1.0"), 410, false)),
        "{rows:?}"
    );
}

#[tokio::test]
async fn upstream_503_is_502_not_404() {
    let up = seed_upstream(&["v1.0.0"]).await;
    let a = spawn_group(&up, Visibility::Public).await;
    up.tap.fail.store(true, Ordering::SeqCst);

    for repo in ["go-proxy", "go-group"] {
        for suffix in ["@v/list", "@latest", "@v/v1.0.0.info", "@v/v1.0.0.zip"] {
            let resp = get(&format!("{}/{repo}/{MODULE}/{suffix}", a.base_url)).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_GATEWAY,
                "{repo} {suffix}: an upstream 503 with no cached copy is 502, so go stops"
            );
        }
    }
    assert_eq!(
        up.tap.count(&up.path(MODULE, "@v/list")),
        2,
        "every miss reached the upstream"
    );
    assert!(cache_rows(&a).await.is_empty(), "failures are never cached");

    up.tap.fail.store(false, Ordering::SeqCst);
    let list = get_text(&format!("{}/go-proxy/{MODULE}/@v/list", a.base_url)).await;
    assert_eq!(list, "v1.0.0", "nothing sticks once the upstream is back");
}
