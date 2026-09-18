//! `opencargo import` from Nexus and Artifactory fakes: listing, npm through
//! the managers' own npm endpoints, cargo off their sparse index, go
//! through their GOPROXY, into a real spawned opencargo.

mod common;

use common::fake_source::manager::{Content, Crate, FakeManager, GoMod, Repo};
use common::fake_source::verdaccio::{Pkg, Ver};
use common::import::{Importer, Run};
use common::{hosted, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};
use reqwest::StatusCode;
use serde_json::{json, Value};

async fn target(repos: &[(&str, RepositoryFormat)]) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: repos.iter().map(|(n, f)| hosted(n, *f, Visibility::Private)).collect(),
        ..Default::default()
    })
    .await
}

async fn get(url: &str) -> (StatusCode, String) {
    let resp = reqwest::Client::new().get(url).bearer_auth(STATIC_TOKEN).send().await.unwrap();
    let status = resp.status();
    (status, resp.text().await.unwrap_or_default())
}

async fn get_json(url: &str) -> (StatusCode, Value) {
    let (s, body) = get(url).await;
    (s, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn kinds(run: &Run) -> Vec<String> {
    let mut k = run.kinds();
    k.dedup();
    k
}

async fn index_lines(t: &TestServer, repo: &str, path: &str) -> Vec<Value> {
    let (status, body) = get(&format!("{}/{repo}/index/{path}", t.base_url)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[tokio::test]
async fn nexus_continuation_token_walks_every_page() {
    let pkgs: Vec<Pkg> = (0..5).map(|i| Pkg::new(&format!("n-{i}"), &["1.0.0"])).collect();
    let src = FakeManager::nexus(vec![Repo::hosted("npm-internal", Content::Npm(pkgs))], 2).await;
    let t = target(&[("npm", RepositoryFormat::Npm)]).await;
    let run = Importer::new()
        .run(&["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--map", "npm-internal=npm"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 5);
    assert!(run.stdout.contains("nexus 3.70.1-02"), "{}", run.stdout);
    let pages = src.log().count(|h| h.path == "/service/rest/v1/components");
    assert_eq!(pages, 3);
    assert!(src.log().hits().iter().filter(|h| h.path == "/service/rest/v1/components").any(|h| h.query.contains("continuationToken=4")));
}

#[tokio::test]
async fn nexus_raw_repository_is_a_gap_not_a_failure() {
    let src = FakeManager::nexus(
        vec![
            Repo::hosted("files", Content::Raw(vec![("docs/a.txt".into(), b"a".to_vec())])),
            Repo::hosted("npm-internal", Content::Npm(vec![Pkg::new("a", &["1.0.0"])])),
        ],
        50,
    )
    .await;
    let t = target(&[("npm", RepositoryFormat::Npm)]).await;
    let imp = Importer::new();
    let args = ["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--map", "npm-internal=npm"];
    let run = imp.run(&args).await;
    assert_eq!(run.code, 4, "{}", run.stdout);
    assert_eq!(kinds(&run), ["UnsupportedFormat"]);
    assert_eq!(run.gaps()[0].1, "files");
    assert_eq!(run.count("copied"), 1);
    assert_eq!(src.log().count(|h| h.query.contains("repository=files")), 0, "an unsupported repository is never listed");
    let accepted = imp.run(&[&args[..], &["--allow-incomplete"]].concat()).await;
    assert_eq!(accepted.code, 0);
}

#[tokio::test]
async fn nexus_proxy_repo_skipped_unless_flagged() {
    let mut cache = Repo::hosted("npm-proxy", Content::Npm(vec![Pkg::new("cached", &["1.0.0"])]));
    cache.kind = "proxy".into();
    let src = FakeManager::nexus(vec![cache], 50).await;
    let t = target(&[("npm", RepositoryFormat::Npm)]).await;
    let args = ["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "npm"];
    let run = Importer::new().run(&args).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(kinds(&run), ["SourceOnlyFeature"]);
    assert_eq!(run.count("copied"), 0);
    let run = Importer::new().run(&[&args[..], &["--include-proxy-caches"]].concat()).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
}

#[tokio::test]
async fn colliding_target_names_are_refused_at_plan_time() {
    let src = FakeManager::nexus(
        vec![
            Repo::hosted("team-a", Content::Npm(vec![Pkg::new("shared", &["1.0.0"])])),
            Repo::hosted("team-b", Content::Npm(vec![Pkg::new("shared", &["1.0.0"])])),
            Repo::hosted("team-c", Content::Npm(vec![Pkg::new("unique", &["1.0.0"])])),
        ],
        50,
    )
    .await;
    let t = target(&[("npm", RepositoryFormat::Npm)]).await;
    let run = Importer::new()
        .run(&["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    let gaps = run.gaps();
    let collision = gaps.iter().find(|g| g.0 == "TargetCollision").expect("a collision row");
    assert!(collision.2.contains("team-a") && collision.2.contains("team-b"), "{collision:?}");
    assert_eq!(run.count("copied"), 1);
    assert_eq!(src.log().count(|h| h.path.contains("shared/-/")), 0, "no byte of either moves");
    assert_eq!(get(&format!("{}/npm/shared", t.base_url)).await.0, StatusCode::NOT_FOUND);
}

fn zlib_line(yanked: bool) -> Value {
    json!({
        "deps": [
            { "name": "libc", "req": "^0.2.150", "features": [], "optional": false, "default_features": true, "target": null, "kind": "normal" },
            { "name": "alias", "package": "real-name", "req": "^1.2", "features": ["x"], "optional": true, "default_features": false,
              "target": "cfg(unix)", "kind": "normal", "registry": "https://other.example/index" },
            { "name": "cc", "req": "^1", "features": [], "optional": false, "default_features": true, "target": null, "kind": "build" }
        ],
        "features": { "default": ["std"], "std": [] },
        "features2": { "fancy": ["dep:alias"] },
        "v": 2,
        "links": "z",
        "rust_version": "1.70",
        "yanked": yanked
    })
}

fn same_dep(a: &Value, b: &Value) -> bool {
    let norm = |d: &Value| {
        let mut d = d.as_object().cloned().unwrap_or_default();
        d.retain(|_, v| !v.is_null());
        d
    };
    norm(a) == norm(b)
}

#[tokio::test]
async fn cargo_index_line_and_yank_come_from_the_source_index() {
    let crates = vec![Crate::new("zlib-sys", "1.0.0", zlib_line(true)), Crate::new("zlib-sys", "1.1.0", zlib_line(false))];
    let source_lines: Vec<Value> = crates.iter().map(|c| c.line.clone()).collect();
    let src = FakeManager::nexus(vec![Repo::hosted("crates", Content::Cargo { crates, index: true })], 50).await;
    let t = target(&[("cargo", RepositoryFormat::Cargo)]).await;
    let run = Importer::new()
        .run(&["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--map", "crates=cargo"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 2);
    let lines = index_lines(&t, "cargo", "zl/ib/zlib-sys").await;
    assert_eq!(lines.len(), 2);
    for (got, want) in lines.iter().zip(&source_lines) {
        assert_eq!(got["vers"], want["vers"]);
        assert_eq!(got["yanked"], want["yanked"], "{} {}", got["vers"], got["yanked"]);
        assert_eq!(got["cksum"], want["cksum"]);
        for key in ["features", "features2", "v", "links", "rust_version"] {
            assert_eq!(got[key], want[key], "{key}");
        }
        let (g, w) = (got["deps"].as_array().unwrap(), want["deps"].as_array().unwrap());
        assert_eq!(g.len(), w.len());
        for wd in w {
            assert!(g.iter().any(|gd| same_dep(gd, wd)), "missing {wd} in {g:?}");
        }
    }
    let (_, detail) = get_json(&format!("{}/api/v1/packages/zlib-sys", t.base_url)).await;
    assert_eq!(detail["description"], "the zlib-sys crate", "{detail}");
}

#[tokio::test]
async fn cargo_index_import_keeps_what_it_can_when_the_manifest_is_unreadable() {
    let crates = vec![Crate::with_manifest("bare", "0.1.0", json!({ "deps": [] }), None)];
    let src = FakeManager::nexus(vec![Repo::hosted("crates", Content::Cargo { crates, index: true })], 50).await;
    let t = target(&[("cargo", RepositoryFormat::Cargo)]).await;
    let run = Importer::new()
        .run(&["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "cargo"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
    let gaps = run.gaps();
    assert!(gaps.iter().any(|g| g.0 == "SourceOnlyFeature" && g.2.contains("description")), "{gaps:?}");
    assert_eq!(index_lines(&t, "cargo", "ba/re/bare").await.len(), 1);
}

#[tokio::test]
async fn cargo_without_a_source_index_falls_back_to_the_manifest() {
    let manifest = r#"
[package]
name = "loner"
version = "0.2.0"
description = "no index"
license = "MIT"
links = "lone"
rust-version = "1.72"

[dependencies]
serde = { version = "^1.0.100", features = ["derive"] }
renamed = { package = "real-dep", version = "^2", optional = true }

[features]
default = []
extra = ["dep:renamed"]
"#;
    let crates = vec![Crate::with_manifest("loner", "0.2.0", json!({}), Some(manifest))];
    let src = FakeManager::artifactory(vec![Repo::hosted("cargo-local", Content::Cargo { crates, index: false })], 1000, true).await;
    let t = target(&[("cargo", RepositoryFormat::Cargo)]).await;
    let run = Importer::new()
        .run(&["--source", "artifactory", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "cargo"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.gaps().iter().any(|g| g.0 == "SourceOnlyFeature" && g.2.contains("yank")), "{:?}", run.gaps());
    let line = &index_lines(&t, "cargo", "lo/ne/loner").await[0];
    assert_eq!(line["links"], "lone");
    assert_eq!(line["rust_version"], "1.72");
    assert_eq!(line["v"], 2);
    assert_eq!(line["features2"], json!({ "extra": ["dep:renamed"] }));
    let deps = line["deps"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["name"] == "serde" && d["req"] == "^1.0.100"), "{deps:?}");
    assert!(deps.iter().any(|d| d["name"] == "renamed" && d["package"] == "real-dep" && d["req"] == "^2"), "{deps:?}");
}

#[tokio::test]
async fn artifactory_aql_offset_pagination_and_repo_types() {
    let pkgs: Vec<Pkg> = (0..3).map(|i| Pkg::new(&format!("a-{i}"), &["1.0.0", "1.1.0"])).collect();
    let mut remote = Repo::hosted("npm-remote", Content::Npm(vec![Pkg::new("far", &["1.0.0"])]));
    remote.kind = "proxy".into();
    let mut virt = Repo::hosted("npm-virtual", Content::Npm(Vec::new()));
    virt.kind = "group".into();
    let src = FakeManager::artifactory(vec![Repo::hosted("npm-local", Content::Npm(pkgs)), remote, virt], 1000, true).await;
    let t = target(&[("npm", RepositoryFormat::Npm)]).await;
    let run = Importer::new()
        .run(&["--source", "artifactory", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "npm", "--page-size", "4"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 6);
    assert!(run.stdout.contains("artifactory 7.104.5"), "{}", run.stdout);
    let gaps = run.gaps();
    assert!(gaps.iter().any(|g| g.1 == "npm-remote" && g.2.contains("remote repository")), "{gaps:?}");
    assert!(gaps.iter().any(|g| g.1 == "npm-virtual" && g.2.contains("virtual")), "{gaps:?}");
    let aql: Vec<_> = src.log().hits().into_iter().filter(|h| h.path == "/api/search/aql").collect();
    assert_eq!(aql.len(), 3, "9 rows at 4 a page");
}

#[tokio::test]
async fn npm_import_from_an_asset_only_source_keeps_dependencies() {
    let mut p = Pkg::new("with-deps", &[]);
    p.versions = vec![Ver::with_deps("with-deps", "1.0.0", json!({ "left-pad": "^1.3.0" }))];
    for npm_api in [true, false] {
        let src = FakeManager::artifactory(vec![Repo::hosted("npm-local", Content::Npm(vec![p.clone()]))], 1000, npm_api).await;
        let t = target(&[("npm", RepositoryFormat::Npm)]).await;
        let run = Importer::new()
            .run(&["--source", "artifactory", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "npm"])
            .await;
        assert_eq!(run.code, 0, "npm api {npm_api}: {}", run.stdout);
        let (_, doc) = get_json(&format!("{}/npm/with-deps", t.base_url)).await;
        assert_eq!(doc["versions"]["1.0.0"]["dependencies"], json!({ "left-pad": "^1.3.0" }), "npm api {npm_api}");
    }
}

#[tokio::test]
async fn go_uppercase_module_path_is_fetchable_by_go_get() {
    let m = GoMod::new("github.com/BurntSushi/toml", "v1.0.0-RC1");
    let src = FakeManager::artifactory(vec![Repo::hosted("go-local", Content::Go(vec![m.clone()]))], 1000, true).await;
    let t = spawn_server(SpawnOpts {
        repositories: vec![hosted("go", RepositoryFormat::Go, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let run = Importer::new()
        .run(&["--source", "artifactory", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "go"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
    let (status, _) = get(&format!("{}/go/github.com/!burnt!sushi/toml/@v/v1.0.0-!r!c1.info", t.base_url)).await;
    assert_eq!(status, StatusCode::OK);

    let escaped = reqwest::Client::new()
        .put(format!("{}/go/github.com/!burnt!sushi/toml/@v/v1.0.0-!r!c1", t.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(m.zip.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(escaped.status(), StatusCode::BAD_REQUEST, "the escaped path is not a publishable one");

    let Some(go) = common::client_bin("GO_BIN") else { return };
    let home = tempfile::tempdir().unwrap();
    let proxy = format!("{}/go", t.base_url);
    let (ok, stdout, stderr) = common::run_cmd(
        &go,
        &["mod", "download", "-json", "github.com/BurntSushi/toml@v1.0.0-RC1"],
        home.path(),
        &[
            ("GOPROXY", proxy.as_ref()),
            ("GOSUMDB", "off".as_ref()),
            ("GOFLAGS", "-mod=mod".as_ref()),
            ("GOMODCACHE", home.path().join("mod").as_os_str()),
            ("GOPATH", home.path().as_os_str()),
            ("HOME", home.path().as_os_str()),
        ],
    )
    .await;
    assert!(ok, "go mod download failed: {stdout}\n{stderr}");
    assert!(stdout.contains("\"Version\": \"v1.0.0-RC1\""), "{stdout}");
}

#[tokio::test]
async fn go_present_without_a_checksum_skips_with_a_note() {
    let src = FakeManager::nexus(vec![{
        let mut r = Repo::hosted("go-proxy", Content::Go(vec![GoMod::new("example.com/mod", "v0.1.0")]));
        r.kind = "proxy".into();
        r
    }], 50)
    .await;
    let t = target(&[("go", RepositoryFormat::Go)]).await;
    let imp = Importer::new();
    let args = ["--source", "nexus", "--from", &src.from(), "--to", &t.base_url, "--target-repo", "go", "--include-proxy-caches"];
    assert_eq!(imp.run(&args).await.code, 0);
    let again = imp.run(&args).await;
    assert_eq!(again.code, 0, "{}", again.stdout);
    assert_eq!(again.count("skipped"), 1);
    assert_eq!(kinds(&again), ["SkippedUnverifiable"]);
}
