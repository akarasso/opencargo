use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::http::header;
use serde_json::json;

use super::*;
use crate::policy::testing::{engine_over, fast};
use crate::policy::Tuning;
use crate::proxy::engine::fixture::Fx;

fn packument(versions: &[(&str, &str)]) -> Value {
    let mut v = json!({ "name": "widget", "versions": {}, "time": {} });
    for (version, tarball) in versions {
        v["versions"][version] =
            json!({ "dist": { "tarball": format!("http://up/widget/-/{tarball}") } });
        v["time"][version] = json!("2026-01-01T00:00:00Z");
    }
    v
}

#[test]
fn npm_version_scoped_and_unscoped() {
    assert_eq!(
        npm_version("@acme/widget", "widget-1.0.0.tgz", None).as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        npm_version("widget", "widget-2.0.0-rc.1.tgz", None).as_deref(),
        Some("2.0.0-rc.1")
    );
    assert_eq!(npm_version("widget", "widget-.tgz", None), None);
    assert_eq!(npm_version("widget", "widget-1.0.0.zip", None), None);
}

#[test]
fn npm_version_from_dist_tarball_when_stem_differs() {
    let p = packument(&[("1.0.0", "renamed-1.0.0.tgz")]);
    assert_eq!(
        npm_version("widget", "renamed-1.0.0.tgz", Some(&p)).as_deref(),
        Some("1.0.0")
    );
    let facts = PackageFacts::parse(&p);
    assert_eq!(
        facts.resolve("widget", "renamed-1.0.0.tgz").as_deref(),
        Some("1.0.0")
    );
    assert!(facts.knows("widget", "renamed-1.0.0.tgz"));
    assert_eq!(
        facts.resolve("widget", "widget-9.9.9.tgz").as_deref(),
        Some("9.9.9")
    );
    assert!(
        !facts.knows("widget", "widget-9.9.9.tgz"),
        "resolved by the stem, absent from the copy"
    );
    let mut undated = p.clone();
    undated["time"] = json!({});
    assert!(
        !PackageFacts::parse(&undated).knows("widget", "renamed-1.0.0.tgz"),
        "present but undated is a miss"
    );
}

#[test]
fn npm_version_none_without_packument_match() {
    let p = packument(&[("1.0.0", "renamed-1.0.0.tgz")]);
    assert_eq!(npm_version("widget", "other-2.0.0.tgz", Some(&p)), None);
    assert_eq!(npm_version("widget", "other-2.0.0.tgz", None), None);
    let facts = PackageFacts::parse(&p);
    assert_eq!(facts.resolve("widget", "other-2.0.0.tgz"), None);
}

#[test]
fn install_scripts_from_flag_or_hooks() {
    let mut p = packument(&[
        ("1.0.0", "w-1.0.0.tgz"),
        ("1.1.0", "w-1.1.0.tgz"),
        ("1.2.0", "w-1.2.0.tgz"),
    ]);
    p["versions"]["1.0.0"]["hasInstallScript"] = json!(true);
    p["versions"]["1.1.0"]["scripts"] = json!({ "postinstall": "node x", "test": "t" });
    p["versions"]["1.2.0"]["scripts"] = json!({ "test": "t" });
    let facts = PackageFacts::parse(&p);
    assert!(facts.versions["1.0.0"].install_scripts);
    assert!(facts.versions["1.1.0"].install_scripts);
    assert!(!facts.versions["1.2.0"].install_scripts);
    assert_eq!(
        facts.versions["1.0.0"].published_at.unwrap().to_rfc3339(),
        "2026-01-01T00:00:00+00:00"
    );
}

fn cfg() -> PolicyConfig {
    PolicyConfig {
        min_release_age: Some("1h".parse().unwrap()),
        ..Default::default()
    }
}

async fn facts_for(
    shared: &Arc<Shared>,
    fx: &Fx,
    filename: &str,
) -> (Option<Arc<PackageFacts>>, &'static str) {
    package_facts(
        shared,
        &cfg(),
        CacheRepo(&fx.repo),
        &fx.up,
        "widget",
        filename,
    )
    .await
}

fn if_none_match_count(fx: &Fx) -> usize {
    fx.hits()
        .iter()
        .filter(|(_, _, h)| h.contains_key(header::IF_NONE_MATCH))
        .count()
}

#[tokio::test]
async fn package_facts_parses_once_per_row() {
    let fx = Fx::new().await;
    fx.set(|s| {
        s.body = packument(&[("1.0.0", "widget-1.0.0.tgz")])
            .to_string()
            .into_bytes();
        s.etag = Some("\"v1\"".into());
        s.delay = Duration::from_millis(200);
    });
    let tuning = Tuning {
        refresh_floor: Duration::from_millis(300),
        ..fast()
    };
    let (engine, _writer) = engine_over(&fx, cfg(), tuning);
    let shared = engine.shared().clone();
    let parses = || shared.npm_parses.load(Ordering::SeqCst);

    let calls = (0..64).map(|_| facts_for(&shared, &fx, "widget-1.0.0.tgz"));
    let results = futures_util::future::join_all(calls).await;
    let first = results[0].0.clone().unwrap();
    assert!(results.iter().all(|(f, source)| {
        Arc::ptr_eq(f.as_ref().unwrap(), &first) && matches!(*source, "fetch" | "cache")
    }));
    assert_eq!(parses(), 1, "one parse for 64 cold callers");
    assert_eq!(fx.hits().len(), 1, "one fetch, singleflighted");

    let (again, source) = facts_for(&shared, &fx, "widget-1.0.0.tgz").await;
    assert!(Arc::ptr_eq(&again.unwrap(), &first));
    assert_eq!(source, "cache");
    assert_eq!(parses(), 1);

    sqlx::query("UPDATE proxy_cache_entries SET fetched_at = datetime('now', '+1 second'), digest = 'bumped'")
        .execute(&fx.pool)
        .await
        .unwrap();
    let (bumped, _) = facts_for(&shared, &fx, "widget-1.0.0.tgz").await;
    assert!(
        !Arc::ptr_eq(&bumped.unwrap(), &first),
        "a bumped row re-parses"
    );
    assert_eq!(parses(), 2);

    let misses = (0..64).map(|_| facts_for(&shared, &fx, "widget-1.1.0.tgz"));
    let results = futures_util::future::join_all(misses).await;
    let sources: Vec<&str> = results.iter().map(|(_, s)| *s).collect();
    assert!(
        results
            .iter()
            .all(|(f, source)| f.is_some() && *source == "not-in-packument"),
        "{sources:?}"
    );
    assert_eq!(
        if_none_match_count(&fx),
        1,
        "one conditional request for 64 concurrent misses"
    );
    assert_eq!(fx.hits().len(), 2);
    assert_eq!(parses(), 2, "a 304 keeps the parsed facts");

    let (_, source) = facts_for(&shared, &fx, "widget-1.1.0.tgz").await;
    assert_eq!(source, "not-in-packument");
    assert_eq!(
        fx.hits().len(),
        2,
        "a second miss inside refresh_floor makes no request"
    );

    tokio::time::sleep(tuning.refresh_floor + Duration::from_millis(20)).await;
    fx.set(|s| {
        s.body = packument(&[("1.0.0", "widget-1.0.0.tgz"), ("1.1.0", "widget-1.1.0.tgz")])
            .to_string()
            .into_bytes();
        s.etag = Some("\"v2\"".into());
    });
    let (fresh, source) = facts_for(&shared, &fx, "widget-1.1.0.tgz").await;
    assert_eq!(source, "refresh");
    assert!(fresh.unwrap().versions.contains_key("1.1.0"));
    assert_eq!(if_none_match_count(&fx), 2);
    assert_eq!(parses(), 3, "a new body parses once more");
}

#[tokio::test]
async fn refresh_floor_survives_a_rewritten_row() {
    let fx = Fx::new().await;
    fx.set(|s| {
        s.body = packument(&[("1.0.0", "widget-1.0.0.tgz")])
            .to_string()
            .into_bytes();
        s.etag = Some("\"v1\"".into());
    });
    let (engine, _writer) = engine_over(&fx, cfg(), fast());
    let shared = engine.shared().clone();
    let (_, source) = facts_for(&shared, &fx, "widget-1.0.0.tgz").await;
    assert_eq!(source, "fetch");

    fx.set(|s| {
        s.body = packument(&[("1.0.0", "widget-1.0.0.tgz"), ("1.1.0", "widget-1.1.0.tgz")])
            .to_string()
            .into_bytes();
        s.etag = Some("\"v2\"".into());
    });
    let (fresh, source) = facts_for(&shared, &fx, "widget-1.1.0.tgz").await;
    assert_eq!(source, "refresh");
    assert!(fresh.unwrap().versions.contains_key("1.1.0"));
    assert_eq!(if_none_match_count(&fx), 1);
    assert_eq!(
        fx.hits().len(),
        2,
        "the refresh was answered 200: the row is rewritten"
    );

    for _ in 0..2 {
        let (facts, source) = facts_for(&shared, &fx, "widget-1.2.0.tgz").await;
        assert_eq!(source, "not-in-packument");
        assert!(facts.unwrap().versions.contains_key("1.1.0"));
    }
    assert_eq!(
        fx.hits().len(),
        2,
        "inside refresh_floor the rewritten row asks nothing more"
    );
}
