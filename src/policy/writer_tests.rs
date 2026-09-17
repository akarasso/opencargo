use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use serde_json::{json, Value};

use super::*;
use crate::db::proxy_cache::CacheEntry;
use crate::policy::rules::PolicyConfig;
use crate::policy::testing::{engine_over, engine_with, fast, pending, repo, scanner, FakeOsv};
use crate::policy::{PolicyEngine, Source, Tuning, QUEUE};
use crate::proxy::engine::fixture::Fx;
use crate::proxy::engine::Cached;
use crate::proxy::UpstreamStrategy;
use crate::registry::oci::upstream::{OciArtifact, OciUpstream};
use crate::registry::resolve::{CacheRepo, Outcome};
use crate::telemetry::vulns::severity::Severity;

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

fn squat() -> PolicyConfig {
    PolicyConfig {
        typosquat: true,
        ..Default::default()
    }
}

fn aged() -> PolicyConfig {
    PolicyConfig {
        min_release_age: Some("48h".parse().unwrap()),
        ..Default::default()
    }
}

async fn rows(fx: &Fx) -> Vec<(String, Option<String>, String)> {
    sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT name, version, date_source FROM policy_resolutions ORDER BY id",
    )
    .fetch_all(&fx.pool)
    .await
    .unwrap()
}

async fn wait_rows(fx: &Fx, n: usize) -> Vec<(String, Option<String>, String)> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let rows = rows(fx).await;
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {n} rows, have {}", rows(fx).await.len());
}

fn cargo(fx: &Fx, name: &str) -> Pending {
    let member = repo(fx.repo.id, &fx.repo.name, Format::Cargo);
    pending(
        &member,
        &fx.up,
        Format::Cargo,
        name,
        Source::Cargo { cksum: "ab".into() },
    )
}

/// A manifest or index landed in the proxy cache through the engine, as
/// the leaf would have; returns the `Cached` the leaf hands over.
async fn landed(fx: &Fx, engine: &PolicyEngine, body: &Value) -> Cached {
    let bytes = body.to_string();
    let digest = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(bytes.as_bytes())
    );
    fx.set(|s| s.body = bytes.clone().into_bytes());
    let a = OciArtifact::Manifest {
        name: "app".into(),
        digest: format!("sha256:{digest}"),
    };
    match engine
        .shared()
        .proxy
        .fetch(&OciUpstream, &fx.up, CacheRepo(&fx.repo), &a)
        .await
    {
        Ok(Outcome::Found(c)) => c,
        other => panic!("{other:?}"),
    }
}

fn manifest(config_digest: &str) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "config": { "digest": format!("sha256:{config_digest}") },
        "layers": [],
        "annotations": { "org.opencontainers.image.created": "2026-01-01T00:00:00Z" }
    })
}

fn index(children: &[&Cached]) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": INDEX_TYPE,
        "manifests": children.iter().map(|c| json!({
            "digest": format!("sha256:{}", c.entry.digest.clone().unwrap()),
            "platform": { "architecture": "amd64" }
        })).collect::<Vec<_>>()
    })
}

fn oci(fx: &Fx, name: &str, version: &str, body: &Cached) -> Pending {
    let member = repo(fx.repo.id, &fx.repo.name, Format::Oci);
    let mut p = pending(
        &member,
        &fx.up,
        Format::Oci,
        name,
        Source::Oci {
            body: body.clone(),
            served: None,
            parsed: None,
        },
    );
    p.version = Some(version.into());
    p
}

async fn served_digests(fx: &Fx) -> Vec<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>("SELECT digest FROM policy_resolutions ORDER BY id")
        .fetch_all(&fx.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn batch_inserts_in_one_transaction() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, squat(), fast());
    tokio::spawn(writer);
    for i in 0..(BATCH * 2 + 3) {
        engine.record(cargo(&fx, &format!("crate-{i}")));
    }
    let rows = wait_rows(&fx, BATCH * 2 + 3).await;
    assert_eq!(rows.len(), BATCH * 2 + 3);
    assert!(rows
        .iter()
        .all(|(_, v, source)| v.as_deref() == Some("1.0.0") && source == "none"));
    let names: Vec<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"crate-0") && names.contains(&"crate-130"));
    assert_eq!(engine.dropped(), 0);
}

#[tokio::test]
async fn flush_emits_one_coalesced_event() {
    let fx = Fx::new().await;
    let tuning = Tuning {
        notify_period: Duration::from_millis(200),
        ..fast()
    };
    let (engine, writer) = engine_over(&fx, squat(), tuning);
    let mut bus = engine.shared().events.subscribe();
    tokio::spawn(writer);
    for i in 0..300 {
        engine.record(cargo(&fx, &format!("crate-{i}")));
    }
    wait_rows(&fx, 300).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut frames = Vec::new();
    let mut stamps = Vec::new();
    while let Ok(event) = bus.try_recv() {
        assert_eq!(event.event_type, "policy.resolution");
        assert_eq!(event.visibility, Visibility::Admin);
        frames.push(event.data.clone());
        stamps.push(chrono::DateTime::parse_from_rfc3339(&event.ts).unwrap());
    }
    assert!(frames.len() >= 2, "an immediate and a trailing frame: {frames:?}");
    for pair in stamps.windows(2) {
        assert!(
            pair[1] - pair[0] >= chrono::Duration::milliseconds(199),
            "one frame per notify period at most: {stamps:?}"
        );
    }
    let total: u64 = frames.iter().map(|f| f["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 300);
    assert!(frames
        .iter()
        .all(|f| f["repo"] == "requested" && f["member"] == fx.repo.name));
    tokio::time::sleep(Duration::from_secs(1)).await;
    engine.record(cargo(&fx, "later"));
    wait_rows(&fx, 301).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let event = bus.try_recv().unwrap();
    assert_eq!(event.data["count"], 1);
    assert!(bus.try_recv().is_err());
}

#[tokio::test]
async fn oci_child_after_index_in_same_burst_is_suppressed() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, aged(), fast());
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let idx = landed(&fx, &engine, &index(&[&child])).await;
    tokio::spawn(writer);
    for _ in 0..50 {
        engine.record(oci(&fx, "app", "latest", &idx));
        engine.record(oci(&fx, "app", "sha256:child", &child));
    }
    let first = wait_rows(&fx, 50).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(first.len(), 50);
    assert!(first
        .iter()
        .all(|(_, v, source)| v.as_deref() == Some("latest") && source == "annotation"));
    let index_digest = format!("sha256:{}", idx.entry.digest.clone().unwrap());
    assert!(served_digests(&fx)
        .await
        .iter()
        .all(|d| d.as_deref() == Some(index_digest.as_str())));
    assert_eq!(rows(&fx).await.len(), 50, "no child row appeared later");
}

#[tokio::test]
async fn child_consumes_key_once() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, squat(), fast());
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let idx = landed(&fx, &engine, &index(&[&child])).await;
    tokio::spawn(writer);
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "sha256:child", &child));
    engine.record(oci(&fx, "app", "sha256:child", &child));
    let first = wait_rows(&fx, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(first[0].1.as_deref(), Some("latest"));
    assert_eq!(first[1].1.as_deref(), Some("sha256:child"));
    assert_eq!(rows(&fx).await.len(), 2);
}

#[tokio::test]
async fn stale_child_key_records_pull() {
    let fx = Fx::new().await;
    let (engine, _writer) = engine_over(&fx, squat(), fast());
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let shared = engine.shared();
    let stale = Instant::now()
        .checked_sub(shared.tuning.child_ttl * 2)
        .unwrap();
    let hex = child.entry.digest.clone().unwrap();
    shared
        .recent_children
        .lock()
        .unwrap()
        .entry((fx.repo.id, "app".into(), hex))
        .or_default()
        .push_back((7, stale));
    let released = facts::oci_classify(shared, oci(&fx, "app", "sha256:child", &child)).await;
    assert!(released.is_some(), "a dead seq never swallows a pull");
    assert!(
        shared.recent_children.lock().unwrap().is_empty(),
        "the key is gone"
    );
}

#[tokio::test]
async fn interleaved_pulls_each_suppress_one_child() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, squat(), fast());
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let idx = landed(&fx, &engine, &index(&[&child])).await;
    tokio::spawn(writer);
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "sha256:child", &child));
    engine.record(oci(&fx, "app", "sha256:child", &child));
    let first = wait_rows(&fx, 2).await;
    assert!(first.iter().all(|(_, v, _)| v.as_deref() == Some("latest")));
    engine.record(oci(&fx, "app", "sha256:child", &child));
    let third = wait_rows(&fx, 3).await;
    assert_eq!(third[2].1.as_deref(), Some("sha256:child"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rows(&fx).await.len(), 3);
}

#[tokio::test]
async fn sibling_of_released_pull_is_suppressed_until_a_later_release() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, squat(), fast());
    let amd = landed(&fx, &engine, &manifest("amd")).await;
    let arm = landed(&fx, &engine, &manifest("arm")).await;
    let idx = landed(&fx, &engine, &index(&[&amd, &arm])).await;
    tokio::spawn(writer);
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "sha256:amd", &amd));
    engine.record(oci(&fx, "app", "sha256:arm", &arm));
    let first = wait_rows(&fx, 1).await;
    assert_eq!(
        first[0].1.as_deref(),
        Some("latest"),
        "skopeo copy --all is one pull"
    );
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "sha256:arm", &arm));
    wait_rows(&fx, 2).await;
    engine.record(oci(&fx, "app", "latest", &idx));
    engine.record(oci(&fx, "app", "sha256:amd", &amd));
    let third = wait_rows(&fx, 3).await;
    assert!(third.iter().all(|(_, v, _)| v.as_deref() == Some("latest")));
    engine.record(oci(&fx, "app", "sha256:amd", &amd));
    let fourth = wait_rows(&fx, 4).await;
    assert_eq!(
        fourth[3].1.as_deref(),
        Some("sha256:amd"),
        "the second pull's amd leftover was superseded by the third release"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rows(&fx).await.len(), 4);
}

#[tokio::test]
async fn child_before_index_records_both() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, aged(), fast());
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let idx = landed(&fx, &engine, &index(&[&child])).await;
    tokio::spawn(writer);
    engine.record(oci(&fx, "app", "sha256:child", &child));
    engine.record(oci(&fx, "app", "latest", &idx));
    let rows = wait_rows(&fx, 2).await;
    assert_eq!(rows[0].1.as_deref(), Some("sha256:child"));
    assert_eq!(rows[0].2, "annotation");
    assert_eq!(rows[1].1.as_deref(), Some("latest"));
    assert_eq!(
        rows[1].2, "index-unpulled",
        "released by the tick with no child"
    );
}

#[tokio::test]
async fn parked_index_released_at_ttl() {
    let fx = Fx::new().await;
    let tuning = Tuning {
        child_ttl: Duration::from_millis(100),
        ..fast()
    };
    let (engine, writer) = engine_over(&fx, aged(), tuning);
    let child = landed(&fx, &engine, &manifest("c1")).await;
    let idx = landed(&fx, &engine, &index(&[&child])).await;
    tokio::spawn(writer);
    let started = Instant::now();
    engine.record(oci(&fx, "app", "latest", &idx));
    let rows = wait_rows(&fx, 1).await;
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(rows[0].2, "index-unpulled");
    let published: Option<String> =
        sqlx::query_scalar("SELECT published_at FROM policy_resolutions")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(published, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !engine.shared().holds_oci_state(),
        "keys and parked indexes are swept"
    );
}

struct Counted<F> {
    inner: Pin<Box<F>>,
    polls: Arc<AtomicUsize>,
}

impl<F: Future<Output = ()>> Future for Counted<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.inner.as_mut().poll(cx)
    }
}

#[tokio::test]
async fn idle_writer_is_not_repolled() {
    let fx = Fx::new().await;
    let (engine, writer) = engine_over(&fx, squat(), Tuning::default());
    tokio::time::pause();
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = Counted {
        inner: Box::pin(writer),
        polls: polls.clone(),
    };
    let handle = tokio::spawn(counted);
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(
        polls.load(Ordering::SeqCst) <= 2,
        "polled {} times",
        polls.load(Ordering::SeqCst)
    );
    drop(engine);
    handle.abort();
}

#[tokio::test]
async fn gather_timeout_writes_unknown_row() {
    let fx = Fx::new().await;
    let tuning = Tuning {
        gather_timeout: Duration::from_millis(300),
        ..fast()
    };
    fx.set(|s| s.delay = Duration::from_secs(3));
    let (engine, writer) = engine_over(&fx, aged(), tuning);
    tokio::spawn(writer);
    let member = repo(fx.repo.id, &fx.repo.name, Format::Npm);
    let started = Instant::now();
    engine.record(pending(
        &member,
        &fx.up,
        Format::Npm,
        "widget",
        Source::Npm {
            filename: "widget-1.0.0.tgz".into(),
            digest: Some("aa".into()),
        },
    ));
    let rows = wait_rows(&fx, 1).await;
    let took = started.elapsed();
    assert_eq!(rows[0].2, "timeout");
    assert_eq!(
        rows[0].1.as_deref(),
        Some("1.0.0"),
        "the stem still names the version"
    );
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(2),
        "{took:?}"
    );
    assert_eq!(
        engine.shared().inflight.available_permits(),
        crate::policy::INFLIGHT
    );
}

#[tokio::test]
async fn full_slots_never_stall_the_tick() {
    let fx = Fx::new().await;
    let tuning = Tuning {
        notify_period: Duration::from_millis(600),
        gather_timeout: Duration::from_secs(10),
        ..fast()
    };
    let (engine, writer) = engine_over(&fx, aged(), tuning);
    let mut bus = engine.shared().events.subscribe();
    tokio::spawn(writer);
    engine.record(cargo(&fx, "first"));
    wait_rows(&fx, 1).await;
    let first = tokio::time::timeout(Duration::from_secs(1), bus.recv())
        .await
        .expect("the first flush emits at once")
        .unwrap();
    assert_eq!(first.data["count"], 1);
    engine.record(cargo(&fx, "second"));
    wait_rows(&fx, 2).await;

    fx.set(|s| s.delay = Duration::from_secs(3));
    let member = repo(fx.repo.id, &fx.repo.name, Format::Npm);
    for i in 0..=crate::policy::INFLIGHT {
        engine.record(pending(
            &member,
            &fx.up,
            Format::Npm,
            &format!("slow-{i}"),
            Source::Npm {
                filename: format!("slow-{i}-1.0.0.tgz"),
                digest: None,
            },
        ));
    }
    let trailing = tokio::time::timeout(Duration::from_secs(2), bus.recv())
        .await
        .expect("the pending frame goes out on the tick while every slot is held")
        .unwrap();
    assert_eq!(trailing.data["count"], 1);
    assert_eq!(engine.shared().inflight.available_permits(), 0);
    let rows = wait_rows(&fx, 2 + crate::policy::INFLIGHT + 1).await;
    let sources: Vec<&str> = rows[2..].iter().map(|(_, _, s)| s.as_str()).collect();
    assert!(sources.iter().all(|s| *s == "failed"), "{sources:?}");
    assert_eq!(engine.dropped(), 0);
}

#[tokio::test]
async fn flush_never_blocks_receive() {
    let fx = Fx::new().await;
    let osv = FakeOsv::start().await;
    let cfg = PolicyConfig {
        osv_severity: Some(Severity::High),
        ..Default::default()
    };
    let (engine, writer) = engine_with(&fx, cfg, fast(), scanner(Some(&osv)));
    tokio::spawn(writer);
    osv.hold_next(Duration::from_secs(5));
    let started = Instant::now();
    engine.record(cargo(&fx, "held"));
    for _ in 0..100 {
        if osv.batches().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(osv.batches(), [1], "the first flush is in flight, held");
    for i in 0..300 {
        engine.record(cargo(&fx, &format!("crate-{i}")));
    }
    let rows = wait_rows(&fx, 300).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "300 rows landed while the first flush was held: {:?}",
        started.elapsed()
    );
    assert!(rows.iter().all(|(name, _, _)| name != "held"));
    assert!(osv.batches().iter().all(|n| *n <= BATCH));
    wait_rows(&fx, 301).await;
    assert!(started.elapsed() >= Duration::from_secs(2));
    let verdicts: Vec<(String, String)> = sqlx::query_as(
        "SELECT rule, verdict FROM policy_verdicts v JOIN policy_resolutions r ON r.id = v.resolution_id WHERE r.name = 'held'",
    )
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    assert_eq!(verdicts, [("osv_severity".to_string(), "pass".to_string())]);
}

#[tokio::test]
#[ignore = "load: 5 000 cold events at 500/s, ~16 s; run through `make test-load`"]
async fn burst_over_cold_rate_drops_nothing() {
    let fx = Fx::new().await;
    fx.set(|s| {
        s.delay = Duration::from_millis(200);
        s.body = br#"{"version":{"created_at":"2026-01-01T00:00:00Z"}}"#.to_vec();
    });
    let tuning = Tuning {
        pacer_period: Duration::from_millis(1),
        ..Tuning::default()
    };
    let (engine, writer) = engine_over(&fx, aged(), tuning);
    tokio::spawn(writer);
    let mut peak_backlog = 0usize;
    let mut clock = tokio::time::interval(Duration::from_millis(2));
    for i in 0..5000 {
        clock.tick().await;
        let mut p = cargo(&fx, "widget");
        p.version = Some(format!("0.0.{i}"));
        engine.record(p);
        peak_backlog = peak_backlog.max(QUEUE - engine.queue_capacity());
    }
    assert_eq!(engine.dropped(), 0);
    assert!(peak_backlog < QUEUE, "{peak_backlog}");
    for _ in 0..600 {
        if rows(&fx).await.len() >= 5000 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("only {} rows written", rows(&fx).await.len());
}

#[test]
fn cache_entry_shape_pins_the_served_digest_column() {
    let entry = CacheEntry {
        id: 1,
        repository_id: 1,
        kind: "oci-manifest".into(),
        cache_key: "sha256/aa".into(),
        status: 200,
        storage_path: None,
        content_type: None,
        etag: None,
        digest: Some("aa".into()),
        size: 0,
        fetched_at: String::new(),
        expires_at: None,
        last_used_at: String::new(),
    };
    let key = OciUpstream.cache_key(&OciArtifact::Manifest {
        name: "app".into(),
        digest: "sha256:aa".into(),
    });
    assert_eq!(key.key, entry.cache_key);
}
