mod common;

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use common::fake_osv::{self, cvss_record};
use common::fake_upstream::cargo as fake_index;
use common::fake_upstream::npm as fake_npm;
use common::fake_upstream::oci::{self as fake_oci, Blob, FakeRegistry, Options};
use common::upstream_tap;
use common::{
    add_token, backdate_version, basic_auth_header, build_npm_publish_body, build_tarball,
    create_user, group, hosted, named_token, policy_rows, policy_verdicts, proxy, proxy_with,
    publish_go_module, report, respawn, rules_of, seed_error_opts, sentinel, sha256_digest,
    spawn_server, user_id, verdict_of, wait_for_policy_rows, PolicyRow, ProxyOpts, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility, VulnScanConfig};
use opencargo::policy::rules::PolicyConfig;
use opencargo::policy::Tuning;
use opencargo::telemetry::vulns::severity::Severity;

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

fn policy(member: &str, cfg: PolicyConfig) -> HashMap<String, PolicyConfig> {
    HashMap::from([(member.to_string(), cfg)])
}

fn aged(age: &str) -> PolicyConfig {
    PolicyConfig {
        min_release_age: Some(age.parse().unwrap()),
        ..Default::default()
    }
}

fn scripts() -> PolicyConfig {
    PolicyConfig {
        install_scripts: true,
        ..Default::default()
    }
}

fn squat() -> PolicyConfig {
    PolicyConfig {
        typosquat: true,
        ..Default::default()
    }
}

async fn get(url: &str, token: Option<&str>) -> reqwest::Response {
    let mut req = Client::new().get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req.send().await.expect("request failed")
}

async fn get_ok(url: &str, token: Option<&str>) {
    let resp = get(url, token).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {url}");
}

fn packument(name: &str, base_url: &str, versions: &[(&str, &str, &str)]) -> Value {
    let mut p = json!({ "name": name, "dist-tags": {}, "versions": {}, "time": {} });
    for (version, tarball, time) in versions {
        p["versions"][*version] = json!({
            "name": name,
            "version": version,
            "dist": { "tarball": format!("{base_url}/{name}/-/{tarball}") }
        });
        p["time"][*version] = json!(time);
    }
    p
}

/// A fake npm holding `widget` with the given `(version, tarball, time)`
/// entries and a tarball body for each.
async fn fake_widget(versions: &[(&str, &str, &str)]) -> fake_npm::FakeNpm {
    let fake = fake_npm::start(
        packument("widget", "http://placeholder", versions),
        "\"v1\"",
    )
    .await;
    for (_, tarball, _) in versions {
        fake.add_tarball(tarball, tarball.as_bytes());
    }
    fake
}

fn npm_proxy(upstream: &str, cfg: PolicyConfig) -> SpawnOpts {
    SpawnOpts {
        repositories: vec![proxy("npm-proxy", RepositoryFormat::Npm, upstream)],
        policy: policy("npm-proxy", cfg),
        ..Default::default()
    }
}

fn tarball_url(server: &TestServer, repo: &str, name: &str, filename: &str) -> String {
    format!("{}/{repo}/{name}/-/{filename}", server.base_url)
}

/// Publish `name@1.0.0` into a hosted npm repository of `server`.
async fn publish_npm(server: &TestServer, repo: &str, name: &str) {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"1.0.0"}}"#));
    let body = build_npm_publish_body(name, "1.0.0", "d", &tarball);
    let resp = Client::new()
        .put(format!("{}/{repo}/{name}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish {name} to {repo}");
}

fn actors(rows: &[PolicyRow]) -> Vec<&str> {
    let mut actors: Vec<&str> = rows.iter().map(|r| r.actor.as_str()).collect();
    actors.sort();
    actors
}

fn entries(report: &Value) -> &[Value] {
    report["entries"].as_array().expect("entries").as_slice()
}

fn entry_actors(report: &Value) -> Vec<&str> {
    let mut actors: Vec<&str> = entries(report)
        .iter()
        .map(|e| e["actor"].as_str().unwrap())
        .collect();
    actors.sort();
    actors
}

/// `(resolutions, would_block, unknown)` of a report's totals.
fn totals(report: &Value) -> (u64, u64, u64) {
    let t = &report["totals"];
    (
        t["resolutions"].as_u64().unwrap(),
        t["would_block"].as_u64().unwrap(),
        t["unknown"].as_u64().unwrap(),
    )
}

fn rule_totals(would_block: u64, unknown: u64, pass: u64, not_applicable: u64) -> Value {
    json!({
        "would_block": would_block,
        "unknown": unknown,
        "pass": pass,
        "not_applicable": not_applicable
    })
}

/// A JSON GET with an optional `Authorization` header value.
async fn get_json(url: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Client::new().get(url);
    if let Some(auth) = auth {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    let resp = req.send().await.expect("request failed");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn erase(server: &TestServer, query: &str, token: &str) -> (StatusCode, Value) {
    let resp = Client::new()
        .delete(format!("{}/api/v1/policy/report?{query}", server.base_url))
        .bearer_auth(token)
        .send()
        .await
        .expect("erase request failed");
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn npm_two_tokens_two_rows_with_dates() {
    let up = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "npm-hosted",
            RepositoryFormat::Npm,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    publish_npm(&up, "npm-hosted", "@acme/widget").await;
    backdate_version(&up, "@acme/widget", "1.0.0", 2).await;
    let tap = upstream_tap::start(&up.base_url).await;
    let a = spawn_server(npm_proxy(
        &format!("{}/npm-hosted", tap.base_url),
        PolicyConfig {
            install_scripts: true,
            ..aged("48h")
        },
    ))
    .await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let dev = named_token(&client, &a.base_url, "alice", "dev-laptop").await;
    let url = tarball_url(&a, "npm-proxy", "@acme/widget", "widget-1.0.0.tgz");
    get_ok(&url, Some(&ci)).await;
    get_ok(&url, Some(&dev)).await;

    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(actors(&rows), ["ci-runner", "dev-laptop"]);
    for row in &rows {
        assert_eq!(row.actor_kind, "token");
        assert_eq!(
            (row.requested_repo.as_str(), row.member_repo.as_str()),
            ("npm-proxy", "npm-proxy")
        );
        assert_eq!(
            (row.format.as_str(), row.name.as_str()),
            ("npm", "@acme/widget")
        );
        assert_eq!(row.version.as_deref(), Some("1.0.0"));
        assert!(
            row.digest.as_deref().is_some_and(|d| d.len() == 64),
            "{:?}",
            row.digest
        );
        let age = chrono::Utc::now() - row.published().expect("dated from the SQLite shape");
        assert!(
            (115..=125).contains(&age.num_minutes()),
            "published {age} ago"
        );
    }
    assert_eq!(rows[0].user_id.unwrap(), user_id(&a, "ci").await);
    assert_eq!(rows[1].user_id.unwrap(), user_id(&a, "alice").await);
    assert_eq!(
        rows[0].date_source, "fetch",
        "the recorder fetched the packument the client never asked for"
    );
    assert_eq!(tap.count("/npm-hosted/@acme/widget"), 1);
    let verdicts = policy_verdicts(&a).await;
    for row in &rows {
        assert!(row.would_block && !row.unknown);
        assert_eq!(
            rules_of(&verdicts, row.id),
            ["install_scripts", "min_release_age"],
            "only the enabled rules leave a verdict"
        );
        assert_eq!(
            verdict_of(&verdicts, row.id, "min_release_age"),
            ("would_block", "published 2h ago, threshold 48h")
        );
        assert_eq!(
            verdict_of(&verdicts, row.id, "install_scripts"),
            ("pass", "no install script")
        );
    }

    let full = report(&a, "").await;
    assert_eq!(full["process"]["dropped_since_start"], 0);
    assert_eq!(totals(&full), (2, 2, 0));
    assert_eq!(
        full["totals"]["by_rule"],
        json!({
            "min_release_age": rule_totals(2, 0, 0, 0),
            "install_scripts": rule_totals(0, 0, 2, 0)
        })
    );
    assert_eq!(entry_actors(&full), ["ci-runner", "dev-laptop"]);
    let newest = &entries(&full)[0];
    assert_eq!(newest["id"], rows[1].id, "newest first");
    for (key, want) in [
        ("requested_repo", json!("npm-proxy")),
        ("member_repo", json!("npm-proxy")),
        ("format", json!("npm")),
        ("name", json!("@acme/widget")),
        ("version", json!("1.0.0")),
        ("digest", json!(rows[1].digest)),
        ("actor", json!("dev-laptop")),
        ("actor_kind", json!("token")),
        ("user_id", json!(rows[1].user_id)),
        ("published_at", json!(rows[1].published_at)),
        ("would_block", json!(true)),
        ("unknown", json!(false)),
    ] {
        assert_eq!(newest[key], want, "{key}");
    }
    let created = chrono::DateTime::parse_from_rfc3339(newest["created_at"].as_str().unwrap())
        .expect("created_at is RFC 3339");
    assert!((chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_seconds() < 60);
    assert_eq!(
        newest["verdicts"],
        json!([
            { "rule": "install_scripts", "verdict": "pass", "reason": "no install script" },
            { "rule": "min_release_age", "verdict": "would_block", "reason": "published 2h ago, threshold 48h" }
        ])
    );

    let hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for query in [
        "repo=npm-proxy",
        "rule=min_release_age",
        "since=1h",
        &format!("since={hour_ago}"),
    ] {
        let kept = report(&a, query).await;
        assert_eq!(totals(&kept), (2, 2, 0), "{query}");
        assert_eq!(entries(&kept).len(), 2, "{query}");
    }
    for query in ["repo=nope", "since=2999-01-01T00:00:00Z"] {
        let none = report(&a, query).await;
        assert_eq!(totals(&none), (0, 0, 0), "{query}");
        assert_eq!(none["totals"]["by_rule"], json!({}), "{query}");
        assert!(entries(&none).is_empty(), "{query}");
    }
    let page = report(&a, "page=2&size=1").await;
    assert_eq!(entries(&page).len(), 1);
    assert_eq!(entries(&page)[0]["id"], rows[0].id);
    assert_eq!(
        (page["page"].as_i64(), page["size"].as_i64()),
        (Some(2), Some(1))
    );

    let scripts = report(&a, "rule=install_scripts").await;
    assert_eq!(
        totals(&scripts),
        (2, 0, 0),
        "the tile follows the filtered rule, not the denormalised columns"
    );
    assert_eq!(
        scripts["totals"]["by_rule"],
        json!({ "install_scripts": rule_totals(0, 0, 2, 0) })
    );
    assert_eq!(entries(&scripts).len(), 2);
    for e in entries(&scripts) {
        assert_eq!(
            (e["would_block"].as_bool(), e["unknown"].as_bool()),
            (Some(false), Some(false))
        );
        assert_eq!(
            e["verdicts"],
            json!([{ "rule": "install_scripts", "verdict": "pass", "reason": "no install script" }])
        );
    }

    let url = format!("{}/api/v1/policy/report", a.base_url);
    let (status, body) = get_json(&format!("{url}?rule=bogus"), Some(&bearer(STATIC_TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    for name in [
        "min_release_age",
        "osv_severity",
        "install_scripts",
        "typosquat",
    ] {
        assert!(err.contains(name), "{err}");
    }
    for bad in ["since=1w", "since=yesterday"] {
        let (status, _) = get_json(&format!("{url}?{bad}"), Some(&bearer(STATIC_TOKEN))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (status, _) = get_json(&url, Some(&bearer(&ci))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the report is admin-only");
    let (status, _) = get_json(&url, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn npm_ci_without_packument_still_dated() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let a = spawn_server(npm_proxy(&fake.base_url, aged("48h"))).await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(
        rows[0].published_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    assert_eq!(rows[0].date_source, "fetch");
    assert_eq!(rows[0].version.as_deref(), Some("1.0.0"));
    assert_eq!(
        fake.packument_hits().len(),
        1,
        "exactly one packument request, the recorder's"
    );
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[0].id, "min_release_age").0,
        "pass"
    );
    assert!(!rows[0].would_block && !rows[0].unknown);
    let r = report(&a, "").await;
    assert_eq!(totals(&r), (1, 0, 0));
    assert_eq!(entries(&r)[0]["published_at"], "2026-01-01T00:00:00Z");
    assert_eq!(entries(&r)[0]["actor_kind"], "anonymous");
    assert_eq!(entries(&r)[0]["user_id"], Value::Null);
}

#[tokio::test]
async fn recorder_miss_never_404s_the_client() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    fake.set_gone(true);
    let a = spawn_server(npm_proxy(&fake.base_url, aged("48h"))).await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(rows[0].published_at, None);
    assert_eq!(rows[0].date_source, "not-found");
    assert_eq!(
        fake.packument_hits().len(),
        1,
        "the recorder asked once and was told 404"
    );
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[0].id, "min_release_age"),
        ("unknown", "no publish date (not-found)")
    );

    fake.set_gone(false);
    let resp = get(&format!("{}/npm-proxy/widget", a.base_url), None).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the recorder's miss left no negative row: the client reaches upstream"
    );
    assert_eq!(fake.packument_hits().len(), 2);
}

#[tokio::test]
async fn npm_version_newer_than_cached_packument_refreshes() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let floor = Duration::from_millis(300);
    let a = spawn_server(SpawnOpts {
        policy_tuning: Some(Tuning {
            refresh_floor: floor,
            ..Tuning::default()
        }),
        ..npm_proxy(&fake.base_url, aged("48h"))
    })
    .await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz"),
        None,
    )
    .await;
    wait_for_policy_rows(&a, 1).await;
    let first_etag = fake.etag();

    fake.add_version("1.1.0", "2026-02-01T00:00:00Z");
    fake.add_tarball("widget-1.1.0.tgz", b"1.1.0");
    assert_ne!(fake.etag(), first_etag);
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.1.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(
        rows[1].published_at.as_deref(),
        Some("2026-02-01T00:00:00Z")
    );
    assert_eq!(rows[1].date_source, "refresh");
    let hits = fake.packument_hits();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[1].if_none_match.as_deref(),
        Some(first_etag.as_str()),
        "conditional"
    );

    fake.add_tarball("widget-1.2.0.tgz", b"1.2.0");
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.2.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 3).await;
    assert_eq!(rows[2].published_at, None);
    assert_eq!(rows[2].date_source, "not-in-packument");
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[2].id, "min_release_age"),
        ("unknown", "no publish date (not-in-packument)")
    );
    assert!(rows[2].unknown && !rows[2].would_block);
    assert_eq!(
        verdict_of(&verdicts, rows[1].id, "min_release_age").0,
        "pass"
    );
    assert_eq!(
        fake.packument_hits().len(),
        2,
        "the 200 refresh rewrote the row; inside refresh_floor it asks nothing more"
    );

    tokio::time::sleep(floor + Duration::from_millis(50)).await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.2.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 4).await;
    assert_eq!(rows[3].date_source, "not-in-packument");
    let hits = fake.packument_hits();
    assert_eq!(hits.len(), 3, "past the floor: one conditional request");
    assert_eq!(
        hits[2].if_none_match.as_deref(),
        Some(fake.etag().as_str()),
        "answered 304"
    );

    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.2.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 5).await;
    assert_eq!(rows[4].date_source, "not-in-packument");
    assert_eq!(
        fake.packument_hits().len(),
        3,
        "inside refresh_floor: 3 requests for 5 pulls"
    );
}

#[tokio::test]
async fn npm_ci_fetch_gated_on_rules() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let url = |a: &TestServer| tarball_url(a, "npm-proxy", "widget", "widget-1.0.0.tgz");

    let a = spawn_server(npm_proxy(&fake.base_url, squat())).await;
    get_ok(&url(&a), None).await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(
        (rows[0].date_source.as_str(), rows[0].version.as_deref()),
        ("none", Some("1.0.0"))
    );
    assert!(
        fake.packument_hits().is_empty(),
        "typosquat needs no packument"
    );
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(rules_of(&verdicts, rows[0].id), ["typosquat"]);
    assert_eq!(verdict_of(&verdicts, rows[0].id, "typosquat").0, "pass");

    let a = respawn(
        a,
        npm_proxy(
            &fake.base_url,
            PolicyConfig {
                fetch_missing_facts: false,
                ..scripts()
            },
        ),
    )
    .await;
    get_ok(&url(&a), None).await;
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(rows[1].date_source, "not-fetched");
    assert!(fake.packument_hits().is_empty());
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[1].id, "install_scripts"),
        ("unknown", "packument not read (not-fetched)")
    );

    let a = respawn(a, npm_proxy(&fake.base_url, scripts())).await;
    get_ok(&url(&a), None).await;
    let rows = wait_for_policy_rows(&a, 3).await;
    assert_eq!(rows[2].date_source, "fetch");
    assert_eq!(fake.packument_hits().len(), 1);
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[2].id, "install_scripts"),
        ("pass", "no install script")
    );
}

#[tokio::test]
async fn npm_tarball_stem_without_name_prefix() {
    let fake = fake_widget(&[("1.0.0", "renamed-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    fake.add_tarball("other-2.0.0.tgz", b"other");
    let a = spawn_server(npm_proxy(&fake.base_url, scripts())).await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "renamed-1.0.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(
        rows[0].version.as_deref(),
        Some("1.0.0"),
        "resolved through dist.tarball"
    );
    assert_eq!(
        rows[0].published_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );

    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "other-2.0.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(rows[1].version, None);
    assert_eq!(rows[1].date_source, "filename-unparsed");
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[1].id, "install_scripts"),
        ("unknown", "version unresolved (filename-unparsed)")
    );
    assert_eq!(
        verdict_of(&verdicts, rows[0].id, "install_scripts").0,
        "pass"
    );
}

#[tokio::test]
async fn npm_install_scripts_from_cached_packument() {
    let mut p = packument(
        "widget",
        "http://placeholder",
        &[
            ("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z"),
            ("1.1.0", "widget-1.1.0.tgz", "2026-01-02T00:00:00Z"),
        ],
    );
    p["versions"]["1.0.0"]["scripts"] = json!({ "postinstall": "node install.js" });
    let fake = fake_npm::start(p, "\"v1\"").await;
    fake.add_tarball("widget-1.0.0.tgz", b"1.0.0");
    fake.add_tarball("widget-1.1.0.tgz", b"1.1.0");
    let a = spawn_server(npm_proxy(&fake.base_url, scripts())).await;
    get_ok(&format!("{}/npm-proxy/widget", a.base_url), None).await;
    for filename in ["widget-1.0.0.tgz", "widget-1.1.0.tgz"] {
        get_ok(&tarball_url(&a, "npm-proxy", "widget", filename), None).await;
    }
    let rows = wait_for_policy_rows(&a, 2).await;
    let verdicts = policy_verdicts(&a).await;
    assert!(rows.iter().all(|r| r.date_source == "cache"));
    let (verdict, reason) = verdict_of(&verdicts, rows[0].id, "install_scripts");
    assert_eq!(verdict, "would_block");
    assert!(reason.contains("postinstall"), "{reason}");
    assert!(rows[0].would_block);
    assert_eq!(
        verdict_of(&verdicts, rows[1].id, "install_scripts"),
        ("pass", "no install script")
    );
    assert!(!rows[1].would_block && !rows[1].unknown);
    assert_eq!(
        fake.packument_hits().len(),
        1,
        "the client's own packument request; the recorder read the cache"
    );
}

fn cargo_proxy(
    fake: &fake_index::FakeIndex,
    cfg: PolicyConfig,
    tuning: Option<Tuning>,
) -> SpawnOpts {
    SpawnOpts {
        repositories: vec![proxy_with(
            "cargo-proxy",
            RepositoryFormat::Cargo,
            &fake.index_url(),
            ProxyOpts {
                dl_allow_private: true,
                ..Default::default()
            },
        )],
        policy: policy("cargo-proxy", cfg),
        policy_tuning: tuning,
        ..Default::default()
    }
}

fn crate_url(server: &TestServer, name: &str, version: &str) -> String {
    format!(
        "{}/cargo-proxy/api/v1/crates/{name}/{version}/download",
        server.base_url
    )
}

#[tokio::test]
async fn cargo_row_dates_from_api_or_unknown() {
    let fake = fake_index::start().await;
    fake.add_crate("widget", "0.1.0", b"widget-bytes");
    let created = (chrono::Utc::now() - chrono::Duration::minutes(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    fake.set_created_at("widget", "0.1.0", &created);
    fake.add_crate("other", "0.1.0", b"other-bytes");
    let a = spawn_server(cargo_proxy(&fake, aged("1h"), None)).await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let dev = named_token(&client, &a.base_url, "alice", "dev-laptop").await;
    get_ok(&crate_url(&a, "widget", "0.1.0"), Some(&ci)).await;
    get_ok(&crate_url(&a, "widget", "0.1.0"), Some(&dev)).await;
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(actors(&rows), ["ci-runner", "dev-laptop"]);
    let verdicts = policy_verdicts(&a).await;
    for row in &rows {
        assert_eq!(row.published_at.as_deref(), Some(created.as_str()));
        assert_eq!(
            (row.format.as_str(), row.name.as_str()),
            ("cargo", "widget")
        );
        assert_eq!(
            verdict_of(&verdicts, row.id, "min_release_age"),
            ("would_block", "published 30m ago, threshold 1h")
        );
        assert!(row.would_block);
        assert_eq!(row.version.as_deref(), Some("0.1.0"));
        assert_eq!(
            row.digest.as_deref(),
            Some(sha256_digest(b"widget-bytes").trim_start_matches("sha256:"))
        );
    }
    let hits = fake.api_hits();
    assert_eq!(hits.len(), 1, "the API is asked once for two pulls");
    assert_eq!(hits[0].path, "/api/v1/crates/widget/0.1.0");
    assert!(hits[0]
        .user_agent
        .as_deref()
        .is_some_and(|ua| ua.starts_with("opencargo/")));

    get_ok(&crate_url(&a, "other", "0.1.0"), None).await;
    let rows = wait_for_policy_rows(&a, 3).await;
    assert_eq!(rows[2].published_at, None);
    assert_eq!(rows[2].date_source, "not-found");
    assert_eq!(rows[2].actor_kind, "anonymous");
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(
        verdict_of(&verdicts, rows[2].id, "min_release_age"),
        ("unknown", "no publish date (not-found)")
    );
    assert!(rows[2].unknown && !rows[2].would_block);
    let r = report(&a, "rule=min_release_age").await;
    assert_eq!(totals(&r), (3, 2, 1));
    assert_eq!(
        r["totals"]["by_rule"],
        json!({ "min_release_age": rule_totals(2, 1, 0, 0) })
    );
    let newest = &entries(&r)[0];
    assert_eq!(
        (newest["name"].as_str(), newest["format"].as_str()),
        (Some("other"), Some("cargo"))
    );
    assert_eq!(
        (
            newest["unknown"].as_bool(),
            newest["published_at"].is_null()
        ),
        (Some(true), true)
    );
    assert_eq!(
        newest["verdicts"][0]["reason"],
        "no publish date (not-found)"
    );
}

#[tokio::test]
async fn cargo_api_is_paced_and_429_is_unknown() {
    let fake = fake_index::start().await;
    for i in 0..9 {
        let name = format!("c{i}");
        fake.add_crate(&name, "0.1.0", name.as_bytes());
        fake.set_created_at(&name, "0.1.0", "2026-03-01T12:00:00Z");
    }
    fake.set_api_latency(Duration::from_millis(100));
    let tuning = Tuning {
        pacer_period: Duration::from_millis(500),
        pacer_cooldown: Duration::from_secs(2),
        ..Tuning::default()
    };
    let a = spawn_server(cargo_proxy(&fake, aged("1h"), Some(tuning))).await;
    for i in 0..6 {
        get_ok(&crate_url(&a, &format!("c{i}"), "0.1.0"), None).await;
    }
    let rows = wait_for_policy_rows(&a, 6).await;
    assert!(rows
        .iter()
        .all(|r| r.published_at.is_some() && r.date_source == "fetch"));
    let mut hits = fake.api_hits();
    hits.sort_by_key(|h| h.started);
    assert_eq!(hits.len(), 6);
    for pair in hits.windows(2) {
        assert!(
            pair[1].started >= pair[0].started + Duration::from_millis(490),
            "paced apart"
        );
        assert!(pair[1].started >= pair[0].ended, "never overlapping");
    }

    fake.set_api_status(429);
    get_ok(&crate_url(&a, "c6", "0.1.0"), None).await;
    let rows = wait_for_policy_rows(&a, 7).await;
    assert_eq!(
        (
            rows[6].published_at.as_deref(),
            rows[6].date_source.as_str()
        ),
        (None, "rate-limited")
    );
    assert_eq!(fake.api_hits().len(), 7);
    assert_eq!(
        verdict_of(&policy_verdicts(&a).await, rows[6].id, "min_release_age"),
        ("unknown", "no publish date (rate-limited)")
    );
    get_ok(&crate_url(&a, "c7", "0.1.0"), None).await;
    let rows = wait_for_policy_rows(&a, 8).await;
    assert_eq!(rows[7].date_source, "rate-limited");
    assert_eq!(fake.api_hits().len(), 7, "under cooldown: no request");

    fake.set_api_status(200);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    get_ok(&crate_url(&a, "c8", "0.1.0"), None).await;
    let rows = wait_for_policy_rows(&a, 9).await;
    assert_eq!(
        rows[8].published_at.as_deref(),
        Some("2026-03-01T12:00:00Z")
    );
    assert_eq!(fake.api_hits().len(), 8);
    assert!(rows
        .iter()
        .all(|r| r.published_at.is_some() || r.date_source != "fetch"));
}

const MODULE: &str = "example.com/org/lib";

#[tokio::test]
async fn go_row_reads_time_from_cached_info() {
    let up = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "go-hosted",
            RepositoryFormat::Go,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    for version in ["v1.0.0", "v1.1.0"] {
        publish_go_module(&client, &up.base_url, "go-hosted", MODULE, version).await;
    }
    let tap = upstream_tap::start(&up.base_url).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy(
            "go-proxy",
            RepositoryFormat::Go,
            &format!("{}/go-hosted", tap.base_url),
        )],
        policy: policy("go-proxy", aged("48h")),
        ..Default::default()
    })
    .await;
    let mut times = Vec::new();
    for version in ["v1.0.0", "v1.1.0"] {
        let info = get(
            &format!("{}/go-proxy/{MODULE}/@v/{version}.info", a.base_url),
            None,
        )
        .await;
        assert_eq!(info.status(), StatusCode::OK);
        let info: Value = info.json().await.unwrap();
        times.push(info["Time"].as_str().unwrap().to_string());
        for ext in ["mod", "zip"] {
            get_ok(
                &format!("{}/go-proxy/{MODULE}/@v/{version}.{ext}", a.base_url),
                None,
            )
            .await;
        }
    }
    let rows = wait_for_policy_rows(&a, 2).await;
    let verdicts = policy_verdicts(&a).await;
    for (row, (version, time)) in rows
        .iter()
        .zip([("v1.0.0", &times[0]), ("v1.1.0", &times[1])])
    {
        assert_eq!((row.format.as_str(), row.name.as_str()), ("go", MODULE));
        assert_eq!(row.version.as_deref(), Some(version));
        assert_eq!(row.published_at.as_deref(), Some(time.as_str()));
        assert_eq!(row.date_source, "cache", "the .info the client fetched");
        assert!(row.digest.is_some());
        let (verdict, reason) = verdict_of(&verdicts, row.id, "min_release_age");
        assert_eq!(verdict, "would_block", "published just now: {reason}");
        assert!(reason.ends_with("threshold 48h"), "{reason}");
    }
    assert_eq!(
        policy_rows(&a).await.len(),
        2,
        ".info and .mod record nothing"
    );
    let r = report(&a, "repo=go-proxy").await;
    assert_eq!(totals(&r), (2, 2, 0));
    let versions: Vec<&str> = entries(&r)
        .iter()
        .map(|e| e["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["v1.1.0", "v1.0.0"]);
    assert!(entries(&r)
        .iter()
        .all(|e| e["format"] == "go" && e["name"] == MODULE));
}

const IMAGE: &str = "team/app";

fn manifest_for(config: &[u8], layer: &[u8], created: Option<&str>) -> Vec<u8> {
    let mut m = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": sha256_digest(config),
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": sha256_digest(layer),
            "size": layer.len()
        }]
    });
    if let Some(created) = created {
        m["annotations"] = json!({ "org.opencontainers.image.created": created });
    }
    serde_json::to_vec(&m).unwrap()
}

/// A manifest whose config blob says `created`; annotated when `annotation`.
fn add_image(
    reg: &FakeRegistry,
    name: &str,
    tag: Option<&str>,
    created: &str,
    annotation: bool,
) -> String {
    let config = json!({ "architecture": "amd64", "os": "linux", "created": created }).to_string();
    let layer = format!("layer-{name}-{created}");
    reg.add_blob(name, Blob::Bytes(config.clone().into_bytes()));
    reg.add_blob(name, Blob::Bytes(layer.clone().into_bytes()));
    let manifest = manifest_for(
        config.as_bytes(),
        layer.as_bytes(),
        annotation.then_some(created),
    );
    reg.add_manifest(name, tag, &manifest, MANIFEST_TYPE)
}

fn oci_proxy(reg: &FakeRegistry, cfg: PolicyConfig, tuning: Option<Tuning>) -> SpawnOpts {
    SpawnOpts {
        repositories: vec![proxy("oci-proxy", RepositoryFormat::Oci, &reg.base_url)],
        policy: policy("oci-proxy", cfg),
        policy_tuning: tuning,
        ..Default::default()
    }
}

fn manifest_url(server: &TestServer, name: &str, reference: &str) -> String {
    format!(
        "{}/v2/oci-proxy/{name}/manifests/{reference}",
        server.base_url
    )
}

async fn head_ok(url: &str, token: Option<&str>) {
    let mut req = Client::new().head(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.expect("request failed");
    assert_eq!(resp.status(), StatusCode::OK, "HEAD {url}");
}

fn upstream_manifest_hits(reg: &FakeRegistry) -> usize {
    reg.hits()
        .iter()
        .filter(|h| h.path.contains("/manifests/"))
        .count()
}

#[tokio::test]
async fn oci_manifest_get_records_head_does_not() {
    let reg = fake_oci::start(Options {
        hub_shape: true,
        ..Default::default()
    })
    .await;
    let digest = add_image(&reg, IMAGE, Some("1.0"), "2026-03-01T00:00:00Z", true);
    let cfg = PolicyConfig {
        osv_severity: Some(Severity::High),
        typosquat: true,
        ..aged("48h")
    };
    let a = spawn_server(oci_proxy(&reg, cfg, None)).await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let dev = named_token(&client, &a.base_url, "alice", "dev-laptop").await;
    for token in [&ci, &dev] {
        head_ok(&manifest_url(&a, IMAGE, "1.0"), Some(token)).await;
        get_ok(&manifest_url(&a, IMAGE, "1.0"), Some(token)).await;
    }
    let rows = wait_for_policy_rows(&a, 2).await;
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(actors(&rows), ["ci-runner", "dev-laptop"]);
    for row in &rows {
        assert_eq!((row.format.as_str(), row.name.as_str()), ("oci", IMAGE));
        assert_eq!(row.version.as_deref(), Some("1.0"));
        assert_eq!(row.digest.as_deref(), Some(digest.as_str()));
        assert_eq!(row.published_at.as_deref(), Some("2026-03-01T00:00:00Z"));
        assert_eq!(row.date_source, "annotation");
        assert_eq!(
            verdict_of(&verdicts, row.id, "osv_severity"),
            ("not_applicable", "oci: no osv ecosystem")
        );
        assert_eq!(
            verdict_of(&verdicts, row.id, "typosquat"),
            (
                "not_applicable",
                "oci: a misspelt official image is never served"
            )
        );
        assert_eq!(verdict_of(&verdicts, row.id, "min_release_age").0, "pass");
        assert!(!row.would_block && !row.unknown);
    }
    assert_eq!(
        upstream_manifest_hits(&reg),
        1,
        "HEAD warmed the cache; nothing else asked"
    );
    let r = report(&a, "repo=oci-proxy").await;
    assert_eq!(totals(&r), (2, 0, 0));
    assert_eq!(
        r["totals"]["by_rule"],
        json!({
            "min_release_age": rule_totals(0, 0, 2, 0),
            "osv_severity": rule_totals(0, 0, 0, 2),
            "typosquat": rule_totals(0, 0, 0, 2)
        })
    );
    let newest = &entries(&r)[0];
    assert_eq!(
        (newest["version"].as_str(), newest["digest"].as_str()),
        (Some("1.0"), Some(digest.as_str()))
    );
    assert_eq!(newest["verdicts"].as_array().unwrap().len(), 3);
    let squat = report(&a, "rule=typosquat").await;
    assert_eq!(totals(&squat), (2, 0, 0));
    assert_eq!(
        entries(&squat)[0]["verdicts"][0]["verdict"],
        "not_applicable"
    );
}

#[tokio::test]
async fn oci_index_pull_is_one_row_dated_from_served_child() {
    let reg = fake_oci::start(Options::default()).await;
    let amd = add_image(&reg, IMAGE, None, "2026-04-01T00:00:00Z", false);
    let arm = add_image(&reg, IMAGE, None, "2026-05-01T00:00:00Z", false);
    let index = reg.add_index(IMAGE, Some("latest"), &[(&amd, "amd64"), (&arm, "arm64")]);
    add_image(
        &reg,
        "team/plain",
        Some("1.0"),
        "2026-06-01T00:00:00Z",
        true,
    );
    let child_ttl = Duration::from_secs(3);
    let tuning = Tuning {
        child_ttl,
        ..Tuning::default()
    };
    let a = spawn_server(oci_proxy(&reg, aged("48h"), Some(tuning))).await;
    let client = Client::new();
    let ta = named_token(&client, &a.base_url, "a", "token-a").await;
    let tb = named_token(&client, &a.base_url, "b", "token-b").await;
    let (by_tag, by_amd, by_arm) = (
        manifest_url(&a, IMAGE, "latest"),
        manifest_url(&a, IMAGE, &amd),
        manifest_url(&a, IMAGE, &arm),
    );

    for _ in 0..20 {
        get_ok(&by_tag, Some(&ta)).await;
        get_ok(&by_amd, Some(&ta)).await;
    }
    get_ok(&by_tag, Some(&ta)).await;
    get_ok(&by_arm, Some(&ta)).await;
    let rows = wait_for_policy_rows(&a, 21).await;
    let shapes: Vec<(Option<&str>, &str, Option<&str>)> = rows
        .iter()
        .map(|r| {
            (
                r.version.as_deref(),
                r.date_source.as_str(),
                r.published_at.as_deref(),
            )
        })
        .collect();
    assert!(
        rows.iter()
            .all(|r| r.version.as_deref() == Some("latest") && r.date_source == "config-blob"),
        "{shapes:?}"
    );
    assert!(rows
        .iter()
        .all(|r| r.digest.as_deref() == Some(index.as_str())));
    let dated = |rows: &[PolicyRow], at: &str| {
        rows.iter()
            .filter(|r| r.published_at.as_deref() == Some(at))
            .count()
    };
    assert_eq!(
        dated(&rows, "2026-04-01T00:00:00Z"),
        20,
        "from the amd64 config"
    );
    assert_eq!(
        dated(&rows, "2026-05-01T00:00:00Z"),
        1,
        "from the arm64 config"
    );

    get_ok(&by_tag, Some(&ta)).await;
    get_ok(&by_tag, Some(&tb)).await;
    get_ok(&by_amd, Some(&ta)).await;
    get_ok(&by_amd, Some(&tb)).await;
    wait_for_policy_rows(&a, 23).await;
    let rows = sentinel(&a, &manifest_url(&a, "team/plain", "1.0"), 24).await;
    assert_eq!(actors(&rows[21..23]), ["token-a", "token-b"]);
    assert_eq!(rows[23].name, "team/plain");

    get_ok(&by_tag, Some(&ta)).await;
    get_ok(&by_amd, Some(&ta)).await;
    wait_for_policy_rows(&a, 25).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    get_ok(&by_amd, Some(&tb)).await;
    let rows = wait_for_policy_rows(&a, 26).await;
    assert_eq!(
        (rows[25].actor.as_str(), rows[25].version.as_deref()),
        ("token-b", Some(amd.as_str()))
    );
    assert_eq!(
        rows[25].published_at.as_deref(),
        Some("2026-04-01T00:00:00Z")
    );

    tokio::time::sleep(child_ttl + Duration::from_millis(500)).await;
    get_ok(&by_arm, Some(&tb)).await;
    let rows = wait_for_policy_rows(&a, 27).await;
    assert_eq!(rows[26].version.as_deref(), Some(arm.as_str()));
    assert_eq!(
        rows[26].published_at.as_deref(),
        Some("2026-05-01T00:00:00Z")
    );
    assert_eq!(
        upstream_manifest_hits(&reg),
        4,
        "the tag, both children and the sentinel, each fetched once by a client; the recorder issued none"
    );
}

#[tokio::test]
async fn oci_index_without_child_is_unknown_after_ttl() {
    let reg = fake_oci::start(Options::default()).await;
    let amd = add_image(&reg, IMAGE, None, "2026-04-01T00:00:00Z", false);
    reg.add_index(IMAGE, Some("latest"), &[(&amd, "amd64")]);
    add_image(
        &reg,
        "team/plain",
        Some("1.0"),
        "2026-06-01T00:00:00Z",
        true,
    );
    let zeroed = add_image(
        &reg,
        "team/zero",
        Some("1.0"),
        "1970-01-01T00:00:00Z",
        false,
    );
    let tuning = Tuning {
        child_ttl: Duration::from_secs(1),
        ..Tuning::default()
    };
    let a = spawn_server(oci_proxy(&reg, aged("48h"), Some(tuning))).await;
    get_ok(&manifest_url(&a, IMAGE, "latest"), None).await;
    let rows = sentinel(&a, &manifest_url(&a, "team/plain", "1.0"), 1).await;
    assert_eq!(rows[0].name, "team/plain", "the index is still parked");
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(rows[1].name, IMAGE);
    assert_eq!(
        (
            rows[1].published_at.as_deref(),
            rows[1].date_source.as_str()
        ),
        (None, "index-unpulled")
    );
    assert_eq!(
        verdict_of(&policy_verdicts(&a).await, rows[1].id, "min_release_age"),
        ("unknown", "no publish date (index-unpulled)")
    );
    assert!(rows[1].unknown);

    get_ok(&manifest_url(&a, "team/zero", "1.0"), None).await;
    let rows = wait_for_policy_rows(&a, 3).await;
    assert_eq!(rows[2].digest.as_deref(), Some(zeroed.as_str()));
    assert_eq!(
        (
            rows[2].published_at.as_deref(),
            rows[2].date_source.as_str()
        ),
        (None, "unset-created")
    );
    assert_eq!(
        verdict_of(&policy_verdicts(&a).await, rows[2].id, "min_release_age"),
        ("unknown", "no publish date (unset-created)")
    );
    let r = report(&a, "rule=min_release_age").await;
    assert_eq!(totals(&r), (3, 0, 2));
    assert_eq!(
        r["totals"]["by_rule"],
        json!({ "min_release_age": rule_totals(0, 2, 1, 0) })
    );
    let newest = &entries(&r)[0];
    assert_eq!(
        (newest["name"].as_str(), newest["unknown"].as_bool()),
        (Some("team/zero"), Some(true))
    );
}

const V3_CRITICAL: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";

#[tokio::test]
async fn osv_rule_uses_fake_osv_and_id_cache() {
    let osv = fake_osv::start().await;
    osv.affect("npm", "widget", "1.0.0", &["GHSA-x"]);
    osv.record(cvss_record("GHSA-x", "CVSS_V3", V3_CRITICAL));
    let versions: Vec<(String, String)> = (0..70)
        .map(|i| (format!("2.0.{i}"), format!("widget-2.0.{i}.tgz")))
        .collect();
    let mut all: Vec<(&str, &str, &str)> =
        vec![("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")];
    all.extend(
        versions
            .iter()
            .map(|(v, f)| (v.as_str(), f.as_str(), "2026-01-01T00:00:00Z")),
    );
    let fake = fake_widget(&all).await;
    let a = spawn_server(SpawnOpts {
        vuln: VulnScanConfig {
            enabled: true,
            osv_base_url: osv.base_url.clone(),
            ..Default::default()
        },
        policy_tuning: Some(Tuning {
            flush_period: Duration::from_secs(2),
            ..Tuning::default()
        }),
        ..npm_proxy(
            &fake.base_url,
            PolicyConfig {
                osv_severity: Some(Severity::High),
                ..Default::default()
            },
        )
    })
    .await;
    let url = |filename: &str| tarball_url(&a, "npm-proxy", "widget", filename);

    get_ok(&url("widget-1.0.0.tgz"), None).await;
    wait_for_policy_rows(&a, 1).await;
    get_ok(&url("widget-1.0.0.tgz"), None).await;
    get_ok(&url("widget-1.0.0.tgz"), None).await;
    let rows = wait_for_policy_rows(&a, 3).await;
    let verdicts = policy_verdicts(&a).await;
    for row in &rows {
        assert_eq!(
            verdict_of(&verdicts, row.id, "osv_severity"),
            ("would_block", "GHSA-x critical >= high")
        );
        assert!(row.would_block);
    }
    assert_eq!(osv.hits("GHSA-x"), 1, "the record is fetched once");
    assert_eq!(
        osv.batches(),
        [1],
        "one querybatch, one query, then the memo"
    );
    let r = report(&a, "rule=osv_severity").await;
    assert_eq!(totals(&r), (3, 3, 0));
    assert_eq!(
        entries(&r)[0]["verdicts"],
        json!([{ "rule": "osv_severity", "verdict": "would_block", "reason": "GHSA-x critical >= high" }])
    );

    let urls: Vec<String> = versions.iter().map(|(_, f)| url(f)).collect();
    futures_util::future::join_all(urls.iter().map(|u| get_ok(u, None))).await;
    let rows = wait_for_policy_rows(&a, 73).await;
    let verdicts = policy_verdicts(&a).await;
    for row in &rows[3..] {
        assert_eq!(
            verdict_of(&verdicts, row.id, "osv_severity"),
            ("pass", "no known vulnerability")
        );
    }
    assert_eq!(
        osv.batches(),
        [1, 64, 6],
        "70 distinct versions cost two POSTs, one per flush, none above 64 queries"
    );

    osv.set_down(true);
    fake.add_version("3.0.0", "2026-01-01T00:00:00Z");
    fake.add_tarball("widget-3.0.0.tgz", b"3");
    get_ok(&url("widget-3.0.0.tgz"), None).await;
    let rows = wait_for_policy_rows(&a, 74).await;
    let verdicts = policy_verdicts(&a).await;
    let (verdict, reason) = verdict_of(&verdicts, rows[73].id, "osv_severity");
    assert_eq!(verdict, "unknown");
    assert!(reason.starts_with("osv unreachable: "), "{reason}");
    assert!(rows[73].unknown);
}

#[tokio::test]
async fn oci_registry_token_row_names_token_and_owner() {
    let reg = fake_oci::start(Options::default()).await;
    add_image(&reg, IMAGE, Some("1.0"), "2026-03-01T00:00:00Z", true);
    let a = spawn_server(oci_proxy(&reg, aged("48h"), None)).await;
    let client = Client::new();
    let api_token = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let resp = client
        .get(format!(
            "{}/v2/token?service=opencargo&scope=repository:oci-proxy/{IMAGE}:pull",
            a.base_url
        ))
        .bearer_auth(&api_token)
        .send()
        .await
        .expect("token request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let registry_token = body["token"].as_str().unwrap().to_string();
    assert!(registry_token.starts_with("ocr_"));
    head_ok(&manifest_url(&a, IMAGE, "1.0"), Some(&registry_token)).await;
    get_ok(&manifest_url(&a, IMAGE, "1.0"), Some(&registry_token)).await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(
        (rows[0].actor.as_str(), rows[0].actor_kind.as_str()),
        ("ci-runner", "token")
    );
    assert_eq!(rows[0].user_id, Some(user_id(&a, "ci").await));
    let r = report(&a, "").await;
    let newest = &entries(&r)[0];
    assert_eq!(
        (newest["actor"].as_str(), newest["actor_kind"].as_str()),
        (Some("ci-runner"), Some("token"))
    );
    assert_eq!(newest["user_id"], json!(rows[0].user_id));
}

#[tokio::test]
async fn hosted_reads_record_nothing() {
    let up = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "npm-hosted",
            RepositoryFormat::Npm,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    publish_npm(&up, "npm-hosted", "@acme/widget").await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            proxy(
                "npm-proxy",
                RepositoryFormat::Npm,
                &format!("{}/npm-hosted", up.base_url),
            ),
            group(
                "npm-all",
                RepositoryFormat::Npm,
                &["npm-hosted", "npm-proxy"],
            ),
        ],
        policy: policy("npm-proxy", squat()),
        ..Default::default()
    })
    .await;
    get_ok(
        &tarball_url(&a, "npm-all", "@acme/widget", "widget-1.0.0.tgz"),
        None,
    )
    .await;
    let rows = wait_for_policy_rows(&a, 1).await;
    assert_eq!(
        (
            rows[0].requested_repo.as_str(),
            rows[0].member_repo.as_str()
        ),
        ("npm-all", "npm-proxy")
    );

    publish_npm(&a, "npm-hosted", "@acme/internal").await;
    get_ok(
        &tarball_url(&a, "npm-hosted", "@acme/internal", "internal-1.0.0.tgz"),
        None,
    )
    .await;
    get_ok(
        &tarball_url(&a, "npm-all", "@acme/internal", "internal-1.0.0.tgz"),
        None,
    )
    .await;
    let rows = sentinel(
        &a,
        &tarball_url(&a, "npm-proxy", "@acme/widget", "widget-1.0.0.tgz"),
        2,
    )
    .await;
    assert_eq!(rows[1].requested_repo, "npm-proxy");
    assert!(
        rows.iter().all(|r| r.name == "@acme/widget"),
        "the hosted pulls left no row"
    );
    assert_eq!(
        totals(&report(&a, "repo=npm-all").await).0,
        1,
        "requested repo"
    );
    assert_eq!(
        totals(&report(&a, "repo=npm-proxy").await).0,
        2,
        "member repo"
    );
    assert_eq!(totals(&report(&a, "repo=npm-hosted").await).0, 0);
}

#[tokio::test]
async fn unconfigured_member_records_nothing() {
    let fake = fake_widget(&[
        ("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z"),
        ("1.1.0", "widget-1.1.0.tgz", "2026-01-02T00:00:00Z"),
    ])
    .await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![proxy("npm-proxy", RepositoryFormat::Npm, &fake.base_url)],
        ..Default::default()
    })
    .await;
    get_ok(
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz"),
        None,
    )
    .await;
    let a = respawn(a, npm_proxy(&fake.base_url, scripts())).await;
    let rows = sentinel(
        &a,
        &tarball_url(&a, "npm-proxy", "widget", "widget-1.1.0.tgz"),
        1,
    )
    .await;
    assert_eq!(
        rows[0].version.as_deref(),
        Some("1.1.0"),
        "the sentinel, not the first pull"
    );
}

#[tokio::test]
async fn policy_key_on_group_refused() {
    let err = seed_error_opts(SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, "http://127.0.0.1:1"),
            group(
                "npm-all",
                RepositoryFormat::Npm,
                &["npm-hosted", "npm-proxy"],
            ),
        ],
        policy: policy("npm-all", squat()),
        ..Default::default()
    })
    .await;
    assert!(err.contains("npm-all") && err.contains("group"), "{err}");
}

#[tokio::test]
async fn rules_endpoint_reports_effective_config() {
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            proxy("npm-proxy", RepositoryFormat::Npm, "http://127.0.0.1:1"),
        ],
        policy: policy(
            "npm-proxy",
            PolicyConfig {
                osv_severity: Some(Severity::High),
                ..aged("48h")
            },
        ),
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let resp = client
        .post(format!("{}/api/v1/repositories", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({
            "name": "later-proxy",
            "type": "proxy",
            "format": "npm",
            "upstream": "http://127.0.0.1:1"
        }))
        .send()
        .await
        .expect("create repository failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let url = format!("{}/api/v1/policy/rules", a.base_url);
    let (status, body) = get_json(&url, Some(&bearer(STATIC_TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["osv_enabled"], false);
    assert_eq!(body["recording"], json!(["npm-proxy"]));
    assert_eq!(
        body["repositories"],
        json!({
            "npm-proxy": {
                "min_release_age": "48h",
                "osv_severity": "high",
                "install_scripts": false,
                "typosquat": false,
                "fetch_missing_facts": true
            },
            "later-proxy": {
                "min_release_age": null,
                "osv_severity": null,
                "install_scripts": false,
                "typosquat": false,
                "fetch_missing_facts": true
            }
        }),
        "every proxy, defaults included, hosted left out"
    );

    let reader = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let (status, _) = get_json(&url, Some(&bearer(&reader))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get_json(&url, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Every string in an audit entry, at any depth.
fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::Object(map) => map.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn erase_user_deletes_rows_and_audits() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let a = spawn_server(npm_proxy(&fake.base_url, squat())).await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let bob = named_token(&client, &a.base_url, "bob", "ci-runner").await;
    let url = tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz");
    for token in [&ci, &ci, &bob, &bob] {
        get_ok(&url, Some(token)).await;
    }
    let rows = wait_for_policy_rows(&a, 4).await;
    assert!(
        rows.iter().all(|r| r.actor == "ci-runner"),
        "homonymous tokens"
    );
    let (ci_id, bob_id) = (user_id(&a, "ci").await, user_id(&a, "bob").await);

    assert_eq!(erase(&a, "user=ci", &bob).await.0, StatusCode::FORBIDDEN);
    assert_eq!(erase(&a, "", STATIC_TOKEN).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        erase(&a, &format!("user=ci&user_id={ci_id}"), STATIC_TOKEN)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        erase(&a, "user=nobody", STATIC_TOKEN).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(policy_rows(&a).await.len(), 4, "nothing erased yet");

    let (status, body) = erase(&a, "user=ci", STATIC_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "deleted": 2 }));
    let rows = policy_rows(&a).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r.user_id == Some(bob_id) && r.actor == "ci-runner"),
        "bob's rows survive although their label is ci-runner too: {rows:#?}"
    );
    let verdicts = policy_verdicts(&a).await;
    assert_eq!(verdicts.len(), 2, "ci's verdicts cascaded, bob's stay");
    assert!(verdicts
        .iter()
        .all(|v| rows.iter().any(|r| r.id == v.resolution_id)));
    assert_eq!(totals(&report(&a, "").await).0, 2);

    let (status, audit) = get_json(
        &format!("{}/api/v1/system/audit", a.base_url),
        Some(&bearer(STATIC_TOKEN)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let erasures: Vec<&Value> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "policy.erase")
        .collect();
    assert_eq!(erasures.len(), 1, "{audit}");
    assert_eq!(erasures[0]["target"], "deleted=2");
    assert_eq!(erasures[0]["username"], "static-token");
    for s in strings(erasures[0]) {
        assert!(
            s != "ci" && !s.contains("ci-runner"),
            "the audit row names nobody: {s}"
        );
    }

    let resp = client
        .delete(format!("{}/api/v1/users/ci", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("delete user failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let (status, body) = erase(&a, &format!("user_id={ci_id}"), STATIC_TOKEN).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "deleted": 0 })),
        "a deleted user is still erasable by id"
    );
    assert_eq!(
        erase(&a, "user=ci", STATIC_TOKEN).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(policy_rows(&a).await.len(), 2);
}

#[tokio::test]
async fn me_policy_shows_only_own_rows() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let a = spawn_server(npm_proxy(&fake.base_url, squat())).await;
    let client = Client::new();
    let alice = create_user(&client, &a.base_url, STATIC_TOKEN, "alice", "reader").await;
    let alice_basic = basic_auth_header("alice", alice["password"].as_str().unwrap());
    let dev_laptop = add_token(&client, &a.base_url, "alice", "dev-laptop").await;
    let ci_runner = named_token(&client, &a.base_url, "bob", "ci-runner").await;
    let bobs_alice = add_token(&client, &a.base_url, "bob", "alice").await;
    let (alice_id, bob_id) = (user_id(&a, "alice").await, user_id(&a, "bob").await);

    let url = tarball_url(&a, "npm-proxy", "widget", "widget-1.0.0.tgz");
    for auth in [
        bearer(&dev_laptop),
        alice_basic.clone(),
        bearer(&ci_runner),
        bearer(&bobs_alice),
        bearer(STATIC_TOKEN),
    ] {
        let (status, _) = get_json(&url, Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
    }
    get_ok(&url, None).await;
    wait_for_policy_rows(&a, 6).await;
    assert_eq!(totals(&report(&a, "").await).0, 6);

    let me = format!("{}/api/v1/me/policy", a.base_url);
    for auth in [bearer(&dev_laptop), alice_basic] {
        let (status, mine) = get_json(&me, Some(&auth)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(entry_actors(&mine), ["alice", "dev-laptop"]);
        assert_eq!(totals(&mine), (2, 0, 0));
        assert!(mine.get("process").is_none(), "the drop counter is admin-only");
        assert!(entries(&mine)
            .iter()
            .all(|e| e["user_id"] == json!(alice_id)));
        let kinds: Vec<(&str, &str)> = entries(&mine)
            .iter()
            .map(|e| {
                (
                    e["actor"].as_str().unwrap(),
                    e["actor_kind"].as_str().unwrap(),
                )
            })
            .collect();
        assert!(kinds.contains(&("alice", "user")) && kinds.contains(&("dev-laptop", "token")));
    }

    let widened =
        format!("{me}?user_id={alice_id}&actor=alice&user=alice&rule=typosquat&repo=npm-proxy");
    for auth in [bearer(&ci_runner), bearer(&bobs_alice)] {
        for url in [&me, &widened] {
            let (status, his) = get_json(url, Some(&auth)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(entry_actors(&his), ["alice", "ci-runner"], "{url}");
            assert!(entries(&his).iter().all(|e| e["user_id"] == json!(bob_id)));
            assert_eq!(totals(&his), (2, 0, 0));
        }
    }

    let (status, statics) = get_json(&me, Some(&bearer(STATIC_TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry_actors(&statics), ["static-token"]);
    assert_eq!(entries(&statics)[0]["actor_kind"], "static");
    assert_eq!(entries(&statics)[0]["user_id"], Value::Null);

    let (status, _) = get_json(&me, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
