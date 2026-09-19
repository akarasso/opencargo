//! `opencargo import` against in-process fakes of the source registries and
//! a real spawned opencargo as the target.

mod common;

use common::fake_source::verdaccio::{Config, FakeVerdaccio, Pkg, Search, Ver};
use common::import::{files_in, Importer};
use common::{hosted, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};
use reqwest::StatusCode;
use serde_json::{json, Value};

async fn target_with(repos: Vec<opencargo::config::RepositoryConfig>) -> TestServer {
    spawn_server(SpawnOpts { repositories: repos, ..Default::default() }).await
}

async fn npm_target() -> TestServer {
    target_with(vec![hosted("npm", RepositoryFormat::Npm, Visibility::Private)]).await
}

async fn get_json(url: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new().get(url).bearer_auth(STATIC_TOKEN).send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn from(src: &FakeVerdaccio) -> String {
    format!("{}/", src.url)
}

/// Listing pages, not counting the probe's one-object search.
fn pages(src: &FakeVerdaccio) -> usize {
    src.log().count(|h| h.path == "/-/v1/search" && !h.query.contains("size=1&"))
}

/// The gap kinds of a run, less the dist-tags a failed version left behind.
fn blocking(run: &common::import::Run) -> Vec<String> {
    let gaps = run.gaps();
    gaps.into_iter().filter(|g| !(g.0 == "SourceOnlyFeature" && g.2.contains("dist-tag"))).map(|g| g.0).collect()
}

#[tokio::test]
async fn verdaccio_search_is_the_primary_listing_then_a_packument_per_package() {
    let mut left = Pkg::new("left-pad", &["1.0.0", "1.1.0", "2.0.0-beta.1"]);
    left.versions[1] = Ver::with_deps("left-pad", "1.1.0", json!({ "lodash": "^4.17.0" }));
    left.dist_tags.push(("beta".into(), "2.0.0-beta.1".into()));
    left.dist_tags.retain(|(t, _)| t != "latest");
    left.dist_tags.push(("latest".into(), "1.1.0".into()));
    let scoped = Pkg::new("@acme/utils", &["0.1.0"]);
    let src = FakeVerdaccio::start(vec![left, scoped], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp.run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 4, "{}", run.stdout);
    assert!(run.stdout.contains("verdaccio 6.1.2"), "{}", run.stdout);

    let (status, doc) = get_json(&format!("{}/npm/left-pad", target.base_url)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["versions"]["1.1.0"]["dependencies"], json!({ "lodash": "^4.17.0" }));
    assert_eq!(doc["versions"]["1.1.0"]["license"], "MIT");
    assert_eq!(doc["dist-tags"], json!({ "latest": "1.1.0", "beta": "2.0.0-beta.1" }));
    assert_eq!(doc["description"], "the left-pad package");
    let (status, _) = get_json(&format!("{}/npm/@acme/utils", target.base_url)).await;
    assert_eq!(status, StatusCode::OK);

    let hits = src.log().hits();
    assert_eq!(pages(&src), 1, "one short page ends the walk");
    assert!(
        hits.iter().filter(|h| !h.path.contains("/-/")).all(|h| h.accept.as_deref() == Some("application/json")),
        "the full packument is asked for, never the abbreviated form"
    );
    assert!(imp.report_path().exists());
    assert!(imp.dir.path().join("report.md").exists());
}

#[tokio::test]
async fn present_same_digest_skips_on_a_second_run() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0", "1.0.1"])], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let args = ["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"];
    assert_eq!(imp.run(&args).await.code, 0);
    let again = imp.run(&args).await;
    assert_eq!(again.code, 0, "{}", again.stdout);
    assert_eq!(again.count("skipped"), 2);
    assert_eq!(again.count("copied"), 0);
}

#[tokio::test]
async fn present_different_digest_fails_without_overwriting() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let other = common::build_tarball(r#"{"name":"a","version":"1.0.0","description":"theirs"}"#);
    let body = common::build_npm_publish_body("a", "1.0.0", "theirs", &other);
    let resp = reqwest::Client::new()
        .put(format!("{}/npm/a", target.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let imp = Importer::new();
    let run = imp.run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(blocking(&run), ["Failed"]);
    assert!(run.gaps().iter().any(|g| g.2.contains("conflict")), "{:?}", run.gaps());
    let (_, doc) = get_json(&format!("{}/npm/a", target.base_url)).await;
    assert_eq!(doc["versions"]["1.0.0"]["description"], "theirs");
}

#[tokio::test]
async fn verdaccio_search_paging_stops_on_a_short_page_not_on_total() {
    let pkgs: Vec<Pkg> = (0..7).map(|i| Pkg::new(&format!("pkg-{i}"), &["1.0.0"])).collect();
    let src = FakeVerdaccio::start(pkgs, Config { search: Search::TotalLie, ..Default::default() }).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--page-size", "2", "--dry-run"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("pending"), 7);
    assert_eq!(pages(&src), 4);
    assert!(run.kinds().is_empty(), "{:?}", run.gaps());
}

#[tokio::test]
async fn verdaccio_clamped_page_repeat_is_a_listing_incomplete_gap() {
    let pkgs: Vec<Pkg> = (0..9).map(|i| Pkg::new(&format!("pkg-{i}"), &["1.0.0"])).collect();
    let src = FakeVerdaccio::start(pkgs, Config { search: Search::ClampAt(4), ..Default::default() }).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let args = ["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--page-size", "2", "--dry-run"];
    let run = imp.run(&args).await;
    assert_eq!(run.code, 4, "{}", run.stdout);
    assert_eq!(run.kinds(), ["ListingIncomplete"]);
    assert!(run.gaps()[0].2.contains("re-served"), "{:?}", run.gaps());
    assert_eq!(run.count("pending"), 6);
    let mut accepted = args.to_vec();
    accepted.push("--allow-incomplete");
    assert_eq!(imp.run(&accepted).await.code, 0);
}

#[tokio::test]
async fn verdaccio_unclamped_source_past_10000_completes_with_no_gap() {
    let src = FakeVerdaccio::start(Vec::new(), Config { synthetic: 10_250, ..Default::default() }).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--dry-run", "--rate", "0"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.kinds().is_empty());
    assert_eq!(run.count("pending"), 10_250);
    assert_eq!(pages(&src), 42);
}

#[tokio::test]
async fn verdaccio_empty_search_returning_zero_objects_is_incomplete_not_done() {
    let target = npm_target().await;
    let hidden = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config { search: Search::Empty, ..Default::default() }).await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&hidden), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 4, "{}", run.stdout);
    assert_eq!(run.kinds(), ["ListingIncomplete"]);

    let empty = FakeVerdaccio::start(Vec::new(), Config { web_data: Some(false), ..Default::default() }).await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&empty), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.kinds().is_empty());
}

#[tokio::test]
async fn verdaccio_probe_records_the_version_from_x_powered_by_and_tolerates_its_absence() {
    let target = npm_target().await;
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config { powered_by: None, ..Default::default() }).await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.stdout.contains("source: verdaccio (version unknown)"), "{}", run.stdout);
}

#[tokio::test]
async fn verdaccio_web_data_404_leaves_the_capability_unset_and_the_run_unaffected() {
    let target = npm_target().await;
    let with = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config { web_data: Some(true), ..Default::default() }).await;
    let run = Importer::new().run(&["--source", "verdaccio", "--from", &from(&with), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert!(run.stdout.contains("verdaccio-web-data"), "{}", run.stdout);
    let without = FakeVerdaccio::start(vec![Pkg::new("b", &["1.0.0"])], Config::default()).await;
    let run = Importer::new().run(&["--source", "verdaccio", "--from", &from(&without), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(!run.stdout.contains("verdaccio-web-data"));
    assert_eq!(run.count("copied"), 1);
}

#[tokio::test]
async fn dry_run_writes_state_and_report_but_no_bytes() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp.run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--dry-run"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(imp.state().exists() && imp.report_path().exists());
    assert_eq!(run.count("pending"), 1);
    assert_eq!(src.log().count(|h| h.path.contains("/-/") && h.path.ends_with(".tgz")), 0);
    let (status, _) = get_json(&format!("{}/npm/a", target.base_url)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let status = imp.sub("status", &[]).await;
    assert!(status.stdout.contains("pending 1"), "{}", status.stdout);
}

#[tokio::test]
async fn npm_legacy_uppercase_name_is_a_name_gap_with_no_request_sent() {
    let src = FakeVerdaccio::start(vec![Pkg::new("JSONStream", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(run.kinds(), ["UnpublishableName"]);
    assert_eq!(src.log().count(|h| h.path.ends_with(".tgz")), 0);
}

#[tokio::test]
async fn checksum_mismatch_publishes_nothing_and_the_run_continues() {
    let mut bad = Pkg::new("bad", &["1.0.0"]);
    bad.versions[0].shasum = Some("0".repeat(40));
    let src = FakeVerdaccio::start(vec![bad, Pkg::new("good", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(blocking(&run), ["Failed"]);
    assert!(run.gaps().iter().any(|g| g.2.contains("checksum mismatch")), "{:?}", run.gaps());
    assert_eq!(run.count("copied"), 1);
    assert_eq!(get_json(&format!("{}/npm/bad", target.base_url)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn artifact_over_max_size_is_too_large_gap_and_spool_is_unlinked() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--max-artifact-size", "64"])
        .await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(blocking(&run), ["TooLarge"]);
    assert_eq!(files_in(&imp.spool()), 0);
}

#[tokio::test]
async fn npm_body_over_target_cap_is_reported_not_truncated() {
    let deps = json!({});
    let small = Ver {
        tarball: common::fake_source::verdaccio::tarball_padded("small", "1.0.0", &deps, None, 3000),
        ..Ver::new("small", "1.0.0")
    };
    let big = Ver {
        tarball: common::fake_source::verdaccio::tarball_padded("big", "1.0.0", &deps, None, 4500),
        ..Ver::new("big", "1.0.0")
    };
    let cap = 6000;
    assert!(big.tarball.len() < cap, "the tarball alone is under the cap; only its base64 is over it");
    let mut p_small = Pkg::new("small", &[]);
    p_small.versions = vec![small];
    let mut p_big = Pkg::new("big", &[]);
    p_big.versions = vec![big];
    let src = FakeVerdaccio::start(vec![p_small, p_big], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--max-npm-body", &cap.to_string()])
        .await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(blocking(&run), ["TooLarge"]);
    assert!(run.gaps().iter().any(|g| g.0 == "TooLarge" && g.1.contains("big")), "{:?}", run.gaps());
    assert_eq!(get_json(&format!("{}/npm/small", target.base_url)).await.0, StatusCode::OK);
    assert_eq!(get_json(&format!("{}/npm/big", target.base_url)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn npm_dist_tag_to_missing_version_is_a_gap() {
    let mut p = Pkg::new("a", &["1.0.0", "1.0.1"]);
    p.versions[1].shasum = Some("0".repeat(40));
    p.dist_tags = vec![("latest".into(), "1.0.0".into()), ("next".into(), "1.0.1".into())];
    let src = FakeVerdaccio::start(vec![p], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 2);
    let gaps = run.gaps();
    assert!(gaps.iter().any(|g| g.0 == "SourceOnlyFeature" && g.2.contains("next points at 1.0.1")), "{gaps:?}");
    let (_, doc) = get_json(&format!("{}/npm/a", target.base_url)).await;
    assert_eq!(doc["dist-tags"], json!({ "latest": "1.0.0" }));
}

#[tokio::test]
async fn npm_import_keeps_the_package_description_and_readme() {
    let mut p = Pkg::new("described", &["1.1.0", "1.0.0"]);
    p.readme = Some("# Described\n\nfrom the packument".into());
    let bare_name = "bare";
    let mut bare = Pkg::new(bare_name, &["1.0.0"]);
    bare.description = None;
    bare.readme = None;
    let src = FakeVerdaccio::start(vec![p, bare], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--concurrency", "1"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    let (_, doc) = get_json(&format!("{}/npm/described", target.base_url)).await;
    assert_eq!(doc["description"], "the described package");
    let (_, search) = get_json(&format!("{}/npm/-/v1/search?text=described", target.base_url)).await;
    assert_eq!(search["objects"][0]["package"]["description"], "the described package", "{search}");
    let gaps = run.gaps();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].0, "SourceOnlyFeature");
    assert!(gaps[0].1.ends_with("/bare"), "{gaps:?}");
}

#[tokio::test]
async fn concurrency_four_workers_copy_each_item_once() {
    let pkgs: Vec<Pkg> = (0..12).map(|i| Pkg::new(&format!("c-{i}"), &["1.0.0"])).collect();
    let src = FakeVerdaccio::start(pkgs, Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--concurrency", "4"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 12);
    assert_eq!(src.log().count(|h| h.path.ends_with(".tgz")), 12);
}

#[tokio::test]
async fn npm_import_of_twentyfive_versions_stays_under_the_target_window() {
    let versions: Vec<String> = (0..25).map(|i| format!("1.0.{i}")).collect();
    let refs: Vec<&str> = versions.iter().map(String::as_str).collect();
    let src = FakeVerdaccio::start(vec![Pkg::new("many", &refs)], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 25);
}

#[tokio::test]
async fn no_credential_survives_into_state_or_report() {
    let mut p = Pkg::new("signed", &["1.0.0"]);
    p.versions[0].tarball_query = Some("X-Amz-Signature=deadbeefsig&X-Amz-Credential=AKIAEXAMPLE".into());
    let src = FakeVerdaccio::start(vec![p], Config::default()).await;
    let target = npm_target().await;
    let with_userinfo = src.url.replace("http://", "http://admin:hunter2@");
    let imp = Importer::new()
        .env("OPENCARGO_IMPORT_SOURCE_USER", "admin")
        .env("OPENCARGO_IMPORT_SOURCE_PASSWORD", "hunter2");
    let refused = imp.run(&["--source", "verdaccio", "--from", &with_userinfo, "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(refused.code, 1);
    assert!(refused.stdout.contains("OPENCARGO_IMPORT_SOURCE_USER"), "{}", refused.stdout);
    assert!(!refused.stdout.contains("hunter2"));

    let run = imp.run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(src.log().hits().iter().any(|h| h.auth.as_deref().is_some_and(|a| a.starts_with("Basic "))));
    let mut blobs = vec![run.stdout.clone(), std::fs::read_to_string(imp.report_path()).unwrap()];
    blobs.push(String::from_utf8_lossy(&std::fs::read(imp.state()).unwrap()).to_string());
    if let Ok(wal) = std::fs::read(format!("{}-wal", imp.state().display())) {
        blobs.push(String::from_utf8_lossy(&wal).to_string());
    }
    for b in blobs {
        for secret in ["hunter2", "deadbeefsig", "AKIAEXAMPLE", "YWRtaW46aHVudGVyMg"] {
            assert!(!b.contains(secret), "{secret} leaked");
        }
    }
}

#[tokio::test]
async fn second_process_on_one_state_file_refuses() {
    use opencargo::ports::import::{ImportJournal, RunHeader};
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let journal = opencargo::adapters::sqlite::import_journal::SqliteImportJournal::open(&imp.state(), true).await.unwrap();
    let header = RunHeader {
        source: "verdaccio".into(),
        source_url: from(&src),
        target_url: target.base_url.clone(),
        opts_json: "{}".into(),
        started_at: chrono::Utc::now(),
        finished_at: None,
        phase: "running".into(),
        owner: None,
    };
    journal.begin(&header, "elsewhere:1", true, chrono::Utc::now()).await.unwrap();
    let run = imp.run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("held by elsewhere:1"), "{}", run.stdout);
}

#[tokio::test]
async fn fail_fast_stops_and_state_resumes() {
    let mut bad = Pkg::new("a-bad", &["1.0.0"]);
    bad.versions[0].shasum = Some("0".repeat(40));
    let pkgs = vec![bad, Pkg::new("b", &["1.0.0"]), Pkg::new("c", &["1.0.0"])];
    let src = FakeVerdaccio::start(pkgs, Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let run = imp
        .run(&["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url, "--target-repo", "npm", "--fail-fast", "--concurrency", "1"])
        .await;
    assert_eq!(run.code, 3, "{}", run.stdout);
    assert_eq!(run.count("failed"), 1);
    assert_eq!(run.count("pending"), 2);
    let mut fixed = Pkg::new("a-bad", &["1.0.0"]);
    fixed.description = Some("fixed".into());
    src.set(fixed);
    let resumed = imp.sub("resume", &[]).await;
    assert_eq!(resumed.code, 0, "{}", resumed.stdout);
    assert_eq!(resumed.count("copied"), 3);
    assert_eq!(pages(&src), 1, "a finished stream is not walked again");
}

#[tokio::test]
async fn fixing_the_cause_and_resuming_clears_the_gap_and_exits_zero() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let imp = Importer::new();
    let base = ["--source", "verdaccio", "--from", &from(&src), "--to", &target.base_url];
    let untargeted = imp.run(&base).await;
    assert_eq!(untargeted.code, 4, "{}", untargeted.stdout);
    assert_eq!(untargeted.kinds(), ["NoTarget"]);
    let mut capped = base.to_vec();
    capped.extend(["--target-repo", "npm", "--max-artifact-size", "64"]);
    let too_large = imp.run(&capped).await;
    assert_eq!(too_large.code, 2, "{}", too_large.stdout);
    assert_eq!(blocking(&too_large), ["TooLarge"]);
    let fixed = imp.sub("resume", &["--max-artifact-size", "1MiB"]).await;
    assert_eq!(fixed.code, 0, "{}", fixed.stdout);
    assert!(fixed.kinds().is_empty(), "{:?}", fixed.gaps());
    assert_eq!(imp.sub("report", &[]).await.code, 0);
}

fn verdaccio_args<'a>(src: &'a str, to: &'a str, repo: &'a str) -> Vec<&'a str> {
    vec!["--source", "verdaccio", "--from", src, "--to", to, "--target-repo", repo]
}

async fn set_grant(target: &TestServer, user: &str, repo: &str, read: bool, write: bool) {
    let resp = reqwest::Client::new()
        .put(format!("{}/api/v1/users/{user}/permissions/{repo}", target.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "can_read": read, "can_write": write }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
}

#[tokio::test]
async fn read_only_target_grant_fails_in_probe_not_per_item() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0", "1.0.1"])], Config::default()).await;
    let target = npm_target().await;
    let client = reqwest::Client::new();
    let token = common::named_token(&client, &target.base_url, "ro", "import").await;
    set_grant(&target, "ro", "npm", true, false).await;
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .run(&verdaccio_args(&from(&src), &target.base_url, "npm"))
        .await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("missing write on npm (source: grant)"), "{}", run.stdout);
    assert_eq!(src.log().count(|h| h.path.ends_with(".tgz")), 0);
    assert_eq!(get_json(&format!("{}/npm/a", target.base_url)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn write_only_grant_on_a_private_repo_is_an_absent_row_not_a_false() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let token = common::named_token(&reqwest::Client::new(), &target.base_url, "wo", "import").await;
    set_grant(&target, "wo", "npm", false, true).await;
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .run(&verdaccio_args(&from(&src), &target.base_url, "npm"))
        .await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("not visible to this token"), "{}", run.stdout);
    assert_eq!(src.log().count(|h| h.path.ends_with(".tgz")), 0);
}

#[tokio::test]
async fn write_only_grant_on_a_public_repo_proceeds() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = target_with(vec![hosted("pub", RepositoryFormat::Npm, Visibility::Public)]).await;
    let token = common::named_token(&reqwest::Client::new(), &target.base_url, "wo", "import").await;
    set_grant(&target, "wo", "pub", false, true).await;
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .run(&verdaccio_args(&from(&src), &target.base_url, "pub"))
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
}

#[tokio::test]
async fn create_repos_then_preflight_still_asserts_write() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = target_with(Vec::new()).await;
    let token = common::named_token(&reqwest::Client::new(), &target.base_url, "reader", "import").await;
    let without_admin = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .run(&[verdaccio_args(&from(&src), &target.base_url, "fresh"), vec!["--create-repos"]].concat())
        .await;
    assert_eq!(without_admin.code, 1, "{}", without_admin.stdout);
    assert!(without_admin.stdout.contains("OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN"), "{}", without_admin.stdout);
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .env("OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN", STATIC_TOKEN)
        .run(&[verdaccio_args(&from(&src), &target.base_url, "fresh"), vec!["--create-repos"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.stdout.contains("created fresh (npm)"), "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
    let (_, perms) = get_json(&format!("{}/api/v1/users/reader/permissions", target.base_url)).await;
    assert!(perms.to_string().contains("fresh"), "{perms}");
}

#[tokio::test]
async fn create_repos_with_an_admin_importing_token_sends_no_grant() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = target_with(Vec::new()).await;
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN", STATIC_TOKEN)
        .run(&[verdaccio_args(&from(&src), &target.base_url, "fresh"), vec!["--create-repos"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);

    let target = spawn_server(SpawnOpts { anonymous_read: false, ..Default::default() }).await;
    let token = common::named_token(&reqwest::Client::new(), &target.base_url, "someone", "import").await;
    let resp = reqwest::Client::new()
        .delete(format!("{}/api/v1/users/someone", target.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", &token)
        .env("OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN", STATIC_TOKEN)
        .run(&[verdaccio_args(&from(&src), &target.base_url, "fresh"), vec!["--create-repos"]].concat())
        .await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    let (_, repos) = get_json(&format!("{}/api/v1/repositories", target.base_url)).await;
    assert!(!repos.to_string().contains("fresh"), "nothing is created before the refusal: {repos}");
}

#[tokio::test]
async fn dry_run_with_create_repos_plans_instead_of_exiting_one() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = target_with(Vec::new()).await;
    let run = Importer::new()
        .run(&[verdaccio_args(&from(&src), &target.base_url, "fresh"), vec!["--create-repos", "--dry-run"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.stdout.contains("would-create fresh (npm)"), "{}", run.stdout);
    let (_, repos) = get_json(&format!("{}/api/v1/repositories", target.base_url)).await;
    assert!(!repos.to_string().contains("fresh"), "{repos}");
}

#[tokio::test]
async fn expired_target_token_is_not_a_missing_write_grant() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = npm_target().await;
    let run = Importer::new()
        .env("OPENCARGO_IMPORT_TARGET_TOKEN", "garbage")
        .run(&verdaccio_args(&from(&src), &target.base_url, "npm"))
        .await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("the target token is not valid"), "{}", run.stdout);
    assert!(!run.stdout.contains("missing write"));
}

#[tokio::test]
async fn target_repo_of_the_wrong_format_exits_one_before_any_item() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"])], Config::default()).await;
    let target = target_with(vec![hosted("crates", RepositoryFormat::Cargo, Visibility::Private)]).await;
    let run = Importer::new().run(&verdaccio_args(&from(&src), &target.base_url, "crates")).await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("crates is a cargo repository"), "{}", run.stdout);
    assert_eq!(src.log().count(|h| h.path.ends_with(".tgz")), 0);
}

#[tokio::test]
async fn proxy_target_repo_exits_one_not_n_failed_rows() {
    let src = FakeVerdaccio::start(vec![Pkg::new("a", &["1.0.0"]), Pkg::new("b", &["1.0.0"])], Config::default()).await;
    let target = target_with(vec![common::proxy("mirror", RepositoryFormat::Npm, "http://127.0.0.1:9/")]).await;
    let run = Importer::new().run(&verdaccio_args(&from(&src), &target.base_url, "mirror")).await;
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stdout.contains("mirror is a proxy repository"), "{}", run.stdout);
    assert_eq!(run.count("failed"), 0);
}

#[tokio::test]
async fn target_503_parks_the_lane_instead_of_failing_every_item() {
    use common::fake_source::target::FakeTarget;
    let pkgs: Vec<Pkg> = (0..4).map(|i| Pkg::new(&format!("q-{i}"), &["1.0.0"])).collect();
    let src = FakeVerdaccio::start(pkgs, Config::default()).await;
    let target = FakeTarget::start(StatusCode::SERVICE_UNAVAILABLE, 3).await;
    let run = Importer::new()
        .run(&[verdaccio_args(&from(&src), &target.url, "npm"), vec!["--retries", "0", "--concurrency", "1"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 4);
    assert_eq!(target.accepted(), 4);

    let src = FakeVerdaccio::start(vec![Pkg::new("z", &["1.0.0"])], Config::default()).await;
    let down = FakeTarget::start(StatusCode::SERVICE_UNAVAILABLE, 100).await;
    let run = Importer::new().run(&[verdaccio_args(&from(&src), &down.url, "npm"), vec!["--retries", "0"]].concat()).await;
    assert_eq!(run.code, 3, "a target that stays down stops the run: {}", run.stdout);
    assert_eq!(run.count("failed"), 0);
    assert_eq!(run.count("pending"), 1);
}

#[tokio::test]
async fn npm_target_429_is_a_queue_not_a_failure() {
    use common::fake_source::target::FakeTarget;
    let pkgs: Vec<Pkg> = (0..3).map(|i| Pkg::new(&format!("r-{i}"), &["1.0.0"])).collect();
    let src = FakeVerdaccio::start(pkgs, Config::default()).await;
    let target = FakeTarget::start(StatusCode::TOO_MANY_REQUESTS, 2).await;
    let run = Importer::new()
        .run(&[verdaccio_args(&from(&src), &target.url, "npm"), vec!["--retries", "0"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(target.accepted(), 3);
    assert_eq!(target.publishes(), 5);
}

#[tokio::test]
async fn target_osv_refusal_becomes_target_refused() {
    let osv = common::fake_osv::start().await;
    osv.affect("npm", "lodash", "4.17.20", &["GHSA-import"]);
    osv.record(common::fake_osv::cvss_record("GHSA-import", "CVSS_V3", "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"));
    let target = spawn_server(SpawnOpts {
        repositories: vec![hosted("npm", RepositoryFormat::Npm, Visibility::Private)],
        vuln: opencargo::config::VulnScanConfig {
            enabled: true,
            block_on_critical: true,
            osv_base_url: osv.base_url.clone(),
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let mut p = Pkg::new("vulnerable", &[]);
    p.versions = vec![Ver::with_deps("vulnerable", "1.0.0", json!({ "lodash": "4.17.20" }))];
    let src = FakeVerdaccio::start(vec![p], Config::default()).await;
    let run = Importer::new().run(&verdaccio_args(&from(&src), &target.base_url, "npm")).await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert_eq!(blocking(&run), ["TargetRefused"]);
    assert!(run.gaps().iter().any(|g| g.2.contains("critical vulnerabilities")), "{:?}", run.gaps());
}
