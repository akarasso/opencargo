mod common;

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use common::fake_upstream::cargo as fake_index;
use common::fake_upstream::npm as fake_npm;
use common::fake_upstream::oci::{self as fake_oci, Blob, FakeRegistry, Options};
use common::upstream_tap;
use common::{
    backdate_version, build_npm_publish_body, build_tarball, group, hosted, named_token,
    policy_rows, proxy, proxy_with, publish_go_module, respawn, seed_error_opts, sentinel,
    sha256_digest, spawn_server, user_id, wait_for_policy_rows, PolicyRow, ProxyOpts, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};
use opencargo::policy::rules::PolicyConfig;
use opencargo::policy::Tuning;

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
}

#[tokio::test]
async fn npm_version_newer_than_cached_packument_refreshes() {
    let fake = fake_widget(&[("1.0.0", "widget-1.0.0.tgz", "2026-01-01T00:00:00Z")]).await;
    let a = spawn_server(npm_proxy(&fake.base_url, aged("48h"))).await;
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
    let hits = fake.packument_hits();
    assert_eq!(hits.len(), 3);
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
    let rows = wait_for_policy_rows(&a, 4).await;
    assert_eq!(rows[3].date_source, "not-in-packument");
    assert_eq!(
        fake.packument_hits().len(),
        3,
        "inside refresh_floor: 3 requests for 4 pulls"
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

    let a = respawn(a, npm_proxy(&fake.base_url, scripts())).await;
    get_ok(&url(&a), None).await;
    let rows = wait_for_policy_rows(&a, 3).await;
    assert_eq!(rows[2].date_source, "fetch");
    assert_eq!(fake.packument_hits().len(), 1);
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
    fake.set_created_at("widget", "0.1.0", "2026-03-01T12:00:00Z");
    fake.add_crate("other", "0.1.0", b"other-bytes");
    let a = spawn_server(cargo_proxy(&fake, aged("1h"), None)).await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let dev = named_token(&client, &a.base_url, "alice", "dev-laptop").await;
    get_ok(&crate_url(&a, "widget", "0.1.0"), Some(&ci)).await;
    get_ok(&crate_url(&a, "widget", "0.1.0"), Some(&dev)).await;
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(actors(&rows), ["ci-runner", "dev-laptop"]);
    for row in &rows {
        assert_eq!(row.published_at.as_deref(), Some("2026-03-01T12:00:00Z"));
        assert_eq!(
            (row.format.as_str(), row.name.as_str()),
            ("cargo", "widget")
        );
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
    for (row, (version, time)) in rows
        .iter()
        .zip([("v1.0.0", &times[0]), ("v1.1.0", &times[1])])
    {
        assert_eq!((row.format.as_str(), row.name.as_str()), ("go", MODULE));
        assert_eq!(row.version.as_deref(), Some(version));
        assert_eq!(row.published_at.as_deref(), Some(time.as_str()));
        assert_eq!(row.date_source, "cache", "the .info the client fetched");
        assert!(row.digest.is_some());
    }
    assert_eq!(
        policy_rows(&a).await.len(),
        2,
        ".info and .mod record nothing"
    );
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
    let a = spawn_server(oci_proxy(&reg, aged("48h"), None)).await;
    let client = Client::new();
    let ci = named_token(&client, &a.base_url, "ci", "ci-runner").await;
    let dev = named_token(&client, &a.base_url, "alice", "dev-laptop").await;
    for token in [&ci, &dev] {
        head_ok(&manifest_url(&a, IMAGE, "1.0"), Some(token)).await;
        get_ok(&manifest_url(&a, IMAGE, "1.0"), Some(token)).await;
    }
    let rows = wait_for_policy_rows(&a, 2).await;
    assert_eq!(actors(&rows), ["ci-runner", "dev-laptop"]);
    for row in &rows {
        assert_eq!((row.format.as_str(), row.name.as_str()), ("oci", IMAGE));
        assert_eq!(row.version.as_deref(), Some("1.0"));
        assert_eq!(row.digest.as_deref(), Some(digest.as_str()));
        assert_eq!(row.published_at.as_deref(), Some("2026-03-01T00:00:00Z"));
        assert_eq!(row.date_source, "annotation");
    }
    assert_eq!(
        upstream_manifest_hits(&reg),
        1,
        "HEAD warmed the cache; nothing else asked"
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
