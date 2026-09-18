#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
//! SQLite-only by design: these assertions guarantee the schema, not the
//! ports (designs-next/ports-and-adapters.md 7.4).

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::upstream_tap::{self, Tap};
use common::{
    build_npm_publish_body, build_tarball, group, hosted, proxy, seed_error, spawn_server,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};

const PKG: &str = "@acme/widget";
const VERSION: &str = "1.0.0";
const UPSTREAM_REPO: &str = "npm-hosted";
const LOOPBACK_UPSTREAM: &str = "http://127.0.0.1:9/";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The admin API of one server, every call authenticated with the static token.
struct Admin {
    base_url: String,
    client: reqwest::Client,
}

impl Admin {
    fn of(server: &TestServer) -> Self {
        Self {
            base_url: server.base_url.clone(),
            client: reqwest::Client::new(),
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut req = self
            .client
            .request(
                method,
                format!("{}/api/v1/repositories{path}", self.base_url),
            )
            .bearer_auth(STATIC_TOKEN);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("admin request failed");
        (resp.status(), resp.text().await.unwrap_or_default())
    }

    async fn create(&self, body: Value) -> (StatusCode, String) {
        self.send(reqwest::Method::POST, "", Some(body)).await
    }

    async fn update(&self, name: &str, body: Value) -> (StatusCode, String) {
        self.send(reqwest::Method::PUT, &format!("/{name}"), Some(body))
            .await
    }

    async fn delete(&self, name: &str) -> (StatusCode, String) {
        self.send(reqwest::Method::DELETE, &format!("/{name}"), None)
            .await
    }

    async fn purge(&self, name: &str) -> StatusCode {
        self.send(reqwest::Method::POST, &format!("/{name}/purge-cache"), None)
            .await
            .0
    }

    async fn get(&self, name: &str) -> (StatusCode, Value) {
        let (status, text) = self
            .send(reqwest::Method::GET, &format!("/{name}"), None)
            .await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

fn create_body(name: &str, repo_type: &str, format: &str) -> Value {
    json!({ "name": name, "type": repo_type, "format": format, "visibility": "public" })
}

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

async fn get_status(url: &str) -> StatusCode {
    reqwest::get(url).await.expect("request failed").status()
}

async fn publish(server: &TestServer, repo: &str, name: &str) {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"{VERSION}"}}"#));
    let body = build_npm_publish_body(name, VERSION, "a package", &tarball);
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{name}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");
}

async fn db(server: &TestServer) -> sqlx::SqlitePool {
    let db_path = server.tmp.path().join("opencargo.db");
    sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database")
}

async fn count(pool: &sqlx::SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count query failed")
}

/// A second opencargo holding `PKG`, fronted by a tap; `url` is the proxy upstream.
struct Upstream {
    _server: TestServer,
    tap: Tap,
}

impl Upstream {
    fn url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}", self.tap.base_url)
    }

    fn packument_hits(&self) -> usize {
        self.tap.count(&format!("/{UPSTREAM_REPO}/{PKG}"))
    }
}

async fn seed_upstream() -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            UPSTREAM_REPO,
            RepositoryFormat::Npm,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    publish(&server, UPSTREAM_REPO, PKG).await;
    let tap = upstream_tap::start(&server.base_url).await;
    Upstream {
        _server: server,
        tap,
    }
}

/// How many cached bodies the proxies hold: every file under an
/// incarnation's `_proxy` segment.
fn cached_files(server: &TestServer) -> usize {
    let mut n = 0;
    let mut pending = vec![server.tmp.path().join("storage/r")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.to_string_lossy().contains("/_proxy/") {
                n += 1;
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// `g{i}` -> `g{i+1}` -> ... -> `g6` -> `h`: `first` names the outermost group.
fn chain(first: usize) -> Vec<RepositoryConfig> {
    let mut repositories = vec![hosted("h", RepositoryFormat::Npm, Visibility::Public)];
    repositories.push(group("g6", RepositoryFormat::Npm, &["h"]));
    for i in (first..6).rev() {
        let inner = format!("g{}", i + 1);
        repositories.push(group(&format!("g{i}"), RepositoryFormat::Npm, &[&inner]));
    }
    repositories
}

/// Five nested groups are the limit on create, update and seed; a sixth
/// only exists as a pre-upgrade row and fails at read time. A member that
/// reaches the group, or the group itself, is refused at write time.
#[tokio::test]
async fn depth_cap_and_cycle_detected() {
    let err = seed_error(chain(1)).await;
    assert!(err.contains("6 groups deep"), "{err}");

    let server = spawn_server(SpawnOpts {
        repositories: chain(2),
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);
    assert_eq!(
        get_status(&format!("{}/g2/{PKG}", server.base_url)).await,
        StatusCode::NOT_FOUND,
        "five nested groups resolve"
    );

    let too_deep = with(create_body("g1", "group", "npm"), "members", json!(["g2"]));
    let (status, body) = admin.create(too_deep).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("6 groups deep"), "{body}");
    let shallow = with(create_body("g1", "group", "npm"), "members", json!(["h"]));
    let (status, body) = admin.create(shallow).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = admin.update("g1", json!({ "members": ["g2"] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("6 groups deep"), "{body}");

    let pool = db(&server).await;
    sqlx::query(r#"UPDATE repositories SET config_json = '{"members":["g2"]}' WHERE name = 'g1'"#)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let resp = reqwest::get(format!("{}/g1/{PKG}", server.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("group nesting depth exceeded"));

    let (status, body) = admin.update("g6", json!({ "members": ["g2"] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("already contains 'g6'"), "{body}");
    let (status, body) = admin.update("g6", json!({ "members": ["g6"] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("its own member"), "{body}");
    let (status, body) = admin
        .create(with(
            create_body("gx", "group", "npm"),
            "members",
            json!(["gx"]),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = admin.update("g6", json!({ "members": ["h"] })).await;
    assert_eq!(status, StatusCode::OK);
}

/// A pre-upgrade group row naming a member of another format skips it and
/// keeps serving; the API refuses to create such a row.
#[tokio::test]
async fn member_format_mismatch_skipped() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("n", RepositoryFormat::Npm, Visibility::Public),
            hosted("c", RepositoryFormat::Cargo, Visibility::Public),
            group("g", RepositoryFormat::Npm, &["n"]),
        ],
        ..Default::default()
    })
    .await;
    publish(&server, "n", PKG).await;

    let pool = db(&server).await;
    sqlx::query(
        r#"UPDATE repositories SET config_json = '{"members":["c","n"]}' WHERE name = 'g'"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    assert_eq!(
        get_status(&format!("{}/g/{PKG}", server.base_url)).await,
        StatusCode::OK
    );

    let (status, body) = Admin::of(&server)
        .create(with(
            create_body("g2", "group", "npm"),
            "members",
            json!(["c"]),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("is cargo, not npm"), "{body}");
}

// ---------------------------------------------------------------------------
// Admin API and seed validation
// ---------------------------------------------------------------------------

async fn create_cases(admin: &Admin) {
    let refused = [
        create_body("p1", "proxy", "npm"),
        with(
            with(
                create_body("p2", "proxy", "npm"),
                "upstream",
                json!(LOOPBACK_UPSTREAM),
            ),
            "members",
            json!(["h"]),
        ),
        with(
            create_body("h2", "hosted", "npm"),
            "upstream",
            json!("https://example.com/"),
        ),
        with(create_body("h3", "hosted", "npm"), "members", json!(["h"])),
        with(
            with(create_body("g1", "group", "npm"), "members", json!(["h"])),
            "upstream",
            json!("https://example.com/"),
        ),
        with(
            create_body("g2", "group", "npm"),
            "members",
            json!(["nope"]),
        ),
        with(create_body("g3", "group", "npm"), "members", json!(["c"])),
        with(
            create_body("p3", "proxy", "npm"),
            "upstream",
            json!("http://169.254.169.254/"),
        ),
        with(
            create_body("p4", "proxy", "npm"),
            "upstream",
            json!("http://0.0.0.0/"),
        ),
        with(
            create_body("p5", "proxy", "npm"),
            "upstream",
            json!("http://[fe80::1]/"),
        ),
    ];
    for body in refused {
        let (status, text) = admin.create(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
    }
    let accepted = [
        with(
            create_body("p6", "proxy", "npm"),
            "upstream",
            json!(LOOPBACK_UPSTREAM),
        ),
        with(
            create_body("g5", "group", "npm"),
            "members",
            json!(["h", "p6"]),
        ),
    ];
    for body in accepted {
        let (status, text) = admin.create(body.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}: {text}");
    }
}

async fn update_cases(admin: &Admin) {
    let refused = [
        ("h", json!({ "upstream": "https://example.com/" })),
        ("h", json!({ "members": ["p6"] })),
        ("p6", json!({ "members": ["h"] })),
        ("p6", json!({ "upstream": "http://169.254.1.1/" })),
        ("g5", json!({ "members": ["nope"] })),
        ("g5", json!({ "members": ["c"] })),
        ("g5", json!({ "members": [] })),
    ];
    for (name, body) in refused {
        let (status, text) = admin.update(name, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name} {body}: {text}");
    }

    let (status, text) = admin.update("g5", json!({ "visibility": "private" })).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (_, repo) = admin.get("g5").await;
    assert_eq!(repo["visibility"], "private");
    assert_eq!(
        repo["config"], r#"{"members":["h","p6"]}"#,
        "members survive a visibility patch"
    );

    let (status, text) = admin.update("g5", json!({ "members": ["p6"] })).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let (_, repo) = admin.get("g5").await;
    assert_eq!(repo["config"], r#"{"members":["p6"]}"#);
}

async fn seed_cases() {
    let hosted_with_upstream = RepositoryConfig {
        upstream: Some(LOOPBACK_UPSTREAM.to_string()),
        ..hosted("h", RepositoryFormat::Npm, Visibility::Public)
    };
    let cases: Vec<(Vec<RepositoryConfig>, &str)> = vec![
        (
            vec![proxy("p", RepositoryFormat::Npm, "ftp://x/")],
            "repository p",
        ),
        (
            vec![group("g", RepositoryFormat::Npm, &["nope"])],
            "group member not found: nope",
        ),
        (
            vec![
                hosted("h", RepositoryFormat::Npm, Visibility::Public),
                group("g", RepositoryFormat::Cargo, &["h"]),
            ],
            "is npm, not cargo",
        ),
        (vec![hosted_with_upstream], "take no upstream"),
        (
            vec![
                group("g1", RepositoryFormat::Npm, &["g2"]),
                group("g2", RepositoryFormat::Npm, &["g1"]),
            ],
            "already contains",
        ),
    ];
    for (repositories, expected) in cases {
        let err = seed_error(repositories).await;
        assert!(err.contains(expected), "expected '{expected}' in: {err}");
    }

    let forward = spawn_server(SpawnOpts {
        repositories: vec![
            group("g", RepositoryFormat::Npm, &["h"]),
            hosted("h", RepositoryFormat::Npm, Visibility::Public),
        ],
        ..Default::default()
    })
    .await;
    let (status, _) = Admin::of(&forward).get("g").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a member listed later in the file is pending, not missing"
    );
}

#[tokio::test]
async fn create_update_seed_validate_members_and_upstream() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("h", RepositoryFormat::Npm, Visibility::Public),
            hosted("c", RepositoryFormat::Cargo, Visibility::Public),
        ],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);
    create_cases(&admin).await;
    update_cases(&admin).await;
    seed_cases().await;
}

#[tokio::test]
async fn repo_name_rule_refused_on_create_and_seed() {
    let server = spawn_server(SpawnOpts::default()).await;
    let admin = Admin::of(&server);
    let long = "x".repeat(65);

    for bad in ["A", "a/b", "a..b", long.as_str()] {
        let (status, text) = admin.create(create_body(bad, "hosted", "npm")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {text}");
        assert!(text.contains("invalid repository name"), "{bad}: {text}");

        let err = seed_error(vec![hosted(bad, RepositoryFormat::Npm, Visibility::Public)]).await;
        assert!(err.contains(&format!("repository {bad}")), "{bad}: {err}");
        assert!(err.contains("invalid repository name"), "{bad}: {err}");
    }

    let max = "x".repeat(64);
    for ok in [
        "npm-all",
        "oci-hosted",
        "npm-private",
        "a.b_c-d",
        max.as_str(),
    ] {
        let (status, text) = admin.create(create_body(ok, "hosted", "npm")).await;
        assert_eq!(status, StatusCode::CREATED, "{ok}: {text}");
    }
}

#[tokio::test]
async fn empty_member_list_refused_on_create() {
    let server = spawn_server(SpawnOpts::default()).await;
    let admin = Admin::of(&server);

    for body in [
        with(create_body("g1", "group", "npm"), "members", json!([])),
        create_body("g2", "group", "npm"),
    ] {
        let (status, text) = admin.create(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
        assert!(text.contains("at least one member"), "{body}: {text}");
    }
}

// ---------------------------------------------------------------------------
// Delete and purge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_then_recreate_never_serves_old_cache() {
    let up = seed_upstream().await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![proxy("p", RepositoryFormat::Npm, &up.url())],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);
    let url = format!("{}/p/{PKG}", server.base_url);

    assert_eq!(get_status(&url).await, StatusCode::OK);
    assert_eq!(up.packument_hits(), 1);
    assert!(cached_files(&server) > 0, "the proxy wrote its cache");

    let (status, text) = admin.delete("p").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(cached_files(&server), 0, "delete reclaims the cache files");
    let pool = db(&server).await;
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await,
        0
    );
    pool.close().await;

    let body = with(
        create_body("p", "proxy", "npm"),
        "upstream",
        json!(up.url()),
    );
    let (status, text) = admin.create(body).await;
    assert_eq!(status, StatusCode::CREATED, "{text}");

    up.tap.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        get_status(&url).await,
        StatusCode::BAD_GATEWAY,
        "nothing stale survives the delete"
    );
    up.tap
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get_status(&url).await, StatusCode::OK);
    assert_eq!(up.packument_hits(), 3, "the recreated proxy fetched again");
}

#[tokio::test]
async fn changing_the_upstream_purges_the_cache() {
    let old = seed_upstream().await;
    let new = seed_upstream().await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![proxy("p", RepositoryFormat::Npm, &old.url())],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);
    let url = format!("{}/p/{PKG}", server.base_url);

    assert_eq!(get_status(&url).await, StatusCode::OK);
    assert_eq!(old.packument_hits(), 1);
    let before = cached_files(&server);
    assert!(before > 0);

    let (status, text) = admin.update("p", json!({ "visibility": "public" })).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(cached_files(&server), before, "a patch that keeps the upstream keeps the cache");

    let (status, text) = admin.update("p", json!({ "upstream": new.url() })).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let pool = db(&server).await;
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM reclaim_candidates").await,
        before as i64,
        "the old upstream's files are enqueued for reclamation"
    );
    pool.close().await;

    assert_eq!(get_status(&url).await, StatusCode::OK);
    assert_eq!(new.packument_hits(), 1, "served by the new upstream");
    assert_eq!(old.packument_hits(), 1, "never the old one");
}

/// A group owns no cache rows or files; deleting it leaves its proxy
/// members' caches alone (`purge-cache` is the way to fan out).
#[tokio::test]
async fn delete_group_keeps_member_caches() {
    let up = seed_upstream().await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            proxy("p", RepositoryFormat::Npm, &up.url()),
            group("g", RepositoryFormat::Npm, &["p"]),
        ],
        ..Default::default()
    })
    .await;
    assert_eq!(get_status(&format!("{}/g/{PKG}", server.base_url)).await, StatusCode::OK);
    let before = cached_files(&server);
    assert!(before > 0);

    let (status, text) = Admin::of(&server).delete("g").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(cached_files(&server), before, "the member's files survive");
    assert_eq!(get_status(&format!("{}/p/{PKG}", server.base_url)).await, StatusCode::OK);
    assert_eq!(up.packument_hits(), 1, "served from the surviving cache");
}

#[tokio::test]
async fn delete_proxy_with_legacy_proxy_cache_meta_row() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![proxy("p", RepositoryFormat::Npm, LOOPBACK_UPSTREAM)],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);

    let pool = db(&server).await;
    sqlx::query(
        "INSERT INTO proxy_cache_meta (repository_id, cache_key)
         SELECT id, 'legacy-key' FROM repositories WHERE name = 'p'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_meta").await,
        1
    );

    let (status, text) = admin.delete("p").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(admin.get("p").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_meta").await,
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn delete_member_of_group_is_409() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("h", RepositoryFormat::Npm, Visibility::Public),
            group("g", RepositoryFormat::Npm, &["h"]),
            group("outer", RepositoryFormat::Npm, &["g"]),
        ],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);

    let (status, text) = admin.delete("h").await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(
        text.contains("group(s) g;"),
        "the holding group is named: {text}"
    );
    let (status, text) = admin.delete("g").await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("outer"), "{text}");

    for name in ["outer", "g", "h"] {
        let (status, text) = admin.delete(name).await;
        assert_eq!(status, StatusCode::OK, "{name}: {text}");
    }
}

#[tokio::test]
async fn purge_group_purges_proxy_members_only() {
    let up = seed_upstream().await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("h", RepositoryFormat::Npm, Visibility::Public),
            proxy("p", RepositoryFormat::Npm, &up.url()),
            group("g", RepositoryFormat::Npm, &["h", "p"]),
            group("outer", RepositoryFormat::Npm, &["g"]),
        ],
        ..Default::default()
    })
    .await;
    let admin = Admin::of(&server);
    publish(&server, "h", "@acme/local").await;
    let local_url = format!("{}/g/@acme/local", server.base_url);
    let proxied_url = format!("{}/g/{PKG}", server.base_url);

    assert_eq!(get_status(&local_url).await, StatusCode::OK);
    assert_eq!(get_status(&proxied_url).await, StatusCode::OK);
    assert_eq!(up.packument_hits(), 1);
    let pool = db(&server).await;
    assert!(count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await >= 1);

    assert_eq!(admin.purge("h").await, StatusCode::BAD_REQUEST);
    assert_eq!(admin.purge("outer").await, StatusCode::OK);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await,
        0
    );
    assert!(
        count(&pool, "SELECT COUNT(*) FROM reclaim_candidates").await >= 1,
        "the proxy member's files are enqueued for reclamation"
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM packages").await,
        1,
        "hosted rows untouched"
    );
    pool.close().await;

    assert_eq!(get_status(&local_url).await, StatusCode::OK);
    assert_eq!(get_status(&proxied_url).await, StatusCode::OK);
    assert_eq!(up.packument_hits(), 2, "the purged member fetched again");
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migrate_013_twice_is_noop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("m.db").display());
    let pool = opencargo::adapters::sqlite::connect(&url).await.unwrap();
    opencargo::adapters::sqlite::migrate::run_all(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO repositories (name, repo_type, format, upstream_url)
         VALUES ('p', 'proxy', 'npm', 'https://registry.npmjs.org')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO proxy_cache_entries (repository_id, kind, cache_key, status)
         VALUES (1, 'npm-metadata', 'left-pad', 200)",
    )
    .execute(&pool)
    .await
    .unwrap();

    opencargo::adapters::sqlite::migrate::run_all(&pool)
        .await
        .unwrap();

    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await,
        1
    );
    let indexes = count(
        &pool,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
         AND name IN ('idx_proxy_cache_entries_expires', 'idx_proxy_cache_entries_last_used')",
    )
    .await;
    assert_eq!(indexes, 2);
    pool.close().await;
}
