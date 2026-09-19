use chrono::Duration;
use tempfile::TempDir;

use super::*;
use crate::adapters::sqlite::SqliteStores;
use crate::domain::governance::{Drift, Effect};
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::ports::mcp::{NameFilter, ProbeRun};

struct Db {
    _tmp: TempDir,
    stores: SqliteStores,
    mcp: SqliteMcpStore,
    mirror: i64,
    team: i64,
}

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-17T12:00:00Z").unwrap().with_timezone(&Utc)
}

async fn db() -> Db {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("t.db")).await.unwrap();
    let repos = stores.repositories();
    let spec = |name: &'static str, kind| RepoSpec {
        name,
        kind,
        format: Format::Mcp,
        visibility: Visibility::Public,
        upstream: (kind == RepoKind::Proxy).then_some("https://registry.example"),
        members: &[],
    };
    let mirror = repos.create(&spec("mirror", RepoKind::Proxy), t0()).await.unwrap().id;
    let team = repos.create(&spec("team", RepoKind::Hosted), t0()).await.unwrap().id;
    let mcp = SqliteMcpStore::new(stores.pool());
    Db { _tmp: tmp, stores, mcp, mirror, team }
}

fn declared(permissions: &str, findings: Vec<NewFinding>) -> NewSurface {
    NewSurface {
        source: SurfaceSource::Declared,
        remote_url: String::new(),
        tools_json: None,
        tools_sha256: None,
        permissions_sha256: permissions.to_string(),
        combined_sha256: format!("{permissions}:-"),
        captured_by: None,
        findings,
    }
}

fn observed(source: SurfaceSource, url: &str, permissions: &str, tools: &str, findings: Vec<NewFinding>) -> NewSurface {
    NewSurface {
        source,
        remote_url: url.to_string(),
        tools_json: Some("[]".into()),
        tools_sha256: Some(tools.to_string()),
        permissions_sha256: permissions.to_string(),
        combined_sha256: format!("{permissions}:{tools}"),
        captured_by: None,
        findings,
    }
}

fn finding(pattern: &str, high: bool, tool: &str) -> NewFinding {
    NewFinding {
        pattern: pattern.into(),
        high,
        promoted_by: None,
        field: "tool.description".into(),
        tool: tool.into(),
        span: (0, 3),
        excerpt: "abc".into(),
    }
}

fn record(repo: i64, name: &str, version: &str, envelope: &str, urls: &[&str], now: DateTime<Utc>) -> RecordWrite {
    RecordWrite {
        repository: repo,
        name: name.into(),
        version: version.into(),
        hosted: false,
        envelope_json: envelope.into(),
        schema_url: None,
        status: "active".into(),
        status_message: None,
        status_changed_at: None,
        is_latest: true,
        take_latest: false,
        published_at: None,
        upstream_updated_at: None,
        package_transports: String::new(),
        remote_transports: String::new(),
        remote_urls: urls.iter().map(|u| u.to_string()).collect(),
        declared: declared("p1", Vec::new()),
        now,
    }
}

fn probe(version_id: i64, url: &str, tools: &str, now: DateTime<Utc>) -> ProbeRun {
    ProbeRun {
        version_id,
        remote_url: url.into(),
        protocol_version: Some("2026-07-28".into()),
        outcome: Ok(observed(SurfaceSource::Probe, url, "p1", tools, Vec::new())),
        now,
    }
}

fn approval(repo: i64, url: &str, tools: Option<&str>, now: DateTime<Utc>) -> NewApproval {
    NewApproval {
        repository: repo,
        skill: false,
        name: "io.github.acme/x".into(),
        version: "1.0.0".into(),
        remote_url: url.into(),
        permissions_sha256: "p1".into(),
        tools_sha256: tools.map(str::to_string),
        combined_sha256: String::new(),
        surface_id: None,
        decision: Decision::Approved,
        decided_by: "admin".into(),
        note: None,
        now,
    }
}

async fn row(d: &Db, addressed: i64) -> CatalogRow {
    d.mcp.version(d.mirror, addressed, "io.github.acme/x", Some("1.0.0")).await.unwrap().unwrap()
}

async fn scalar(d: &Db, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&d.stores.pool()).await.unwrap()
}

#[tokio::test]
async fn resync_of_an_unchanged_record_keeps_the_row_id_its_surfaces_and_its_findings() {
    let d = db().await;
    let mut r = record(d.mirror, "io.github.acme/x", "1.0.0", "{\"a\":1}", &["https://a"], t0());
    r.declared.findings = vec![finding("model_directive", true, "")];
    let first = d.mcp.upsert_record(&r).await.unwrap();
    assert!(first.changed);
    d.mcp.record_probe(&probe(first.version_id, "https://a", "t1", t0())).await.unwrap();
    let clock = row(&d, d.mirror).await;

    let mut again = r.clone();
    again.now = t0() + Duration::hours(1);
    let second = d.mcp.upsert_record(&again).await.unwrap();
    assert_eq!(second, Upserted { version_id: first.version_id, changed: false });
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_surfaces").await, 2);
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_findings").await, 1, "a rescan does not duplicate findings");
    let changed_at: String = sqlx::query_scalar("SELECT row_changed_at FROM mcp_server_versions")
        .fetch_one(&d.stores.pool())
        .await
        .unwrap();
    assert_eq!(changed_at, bind_ts(t0()), "the clock moves only when the record does");
    let page = d
        .mcp
        .page(&PageQuery {
            member: d.mirror,
            addressed: d.mirror,
            limit: 10,
            updated_since: Some(t0() + Duration::minutes(30)),
            ..PageQuery::default()
        })
        .await
        .unwrap();
    assert!(page.is_empty(), "an unchanged resync is not a change");
    assert_eq!(row(&d, d.mirror).await.current, clock.current);

    let mut moved = again.clone();
    moved.envelope_json = "{\"a\":2}".into();
    moved.now = t0() + Duration::hours(2);
    assert!(d.mcp.upsert_record(&moved).await.unwrap().changed);
    let page = d
        .mcp
        .page(&PageQuery {
            member: d.mirror,
            addressed: d.mirror,
            limit: 10,
            updated_since: Some(t0() + Duration::minutes(90)),
            ..PageQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
}

#[tokio::test]
async fn a_poisoned_description_still_counts_after_a_probe_surface_becomes_current() {
    let d = db().await;
    let mut r = record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a"], t0());
    r.declared.findings = vec![finding("model_directive", true, "")];
    let v = d.mcp.upsert_record(&r).await.unwrap().version_id;
    d.mcp.record_probe(&probe(v, "https://a", "t1", t0())).await.unwrap();
    let served = row(&d, d.mirror).await;
    assert_eq!(served.current.unwrap().source, SurfaceSource::Probe);
    assert_eq!(served.findings_high, 1);
}

#[tokio::test]
async fn a_fixed_tool_description_clears_the_finding_on_the_next_probe() {
    let d = db().await;
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a"], t0())).await.unwrap().version_id;
    let mut poisoned = probe(v, "https://a", "bad", t0());
    if let Ok(s) = &mut poisoned.outcome {
        s.findings = vec![finding("invisible_chars", true, "search")];
    }
    d.mcp.record_probe(&poisoned).await.unwrap();
    assert_eq!(row(&d, d.mirror).await.findings_high, 1);
    d.mcp.record_probe(&probe(v, "https://a", "good", t0() + Duration::hours(1))).await.unwrap();
    assert_eq!(row(&d, d.mirror).await.findings_high, 0);
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_findings").await, 0, "the superseded surface took its findings");
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_surfaces WHERE source = 'probe'").await, 1);
}

#[tokio::test]
async fn an_unchanged_reprobe_advances_captured_at_and_a_failure_invents_no_hashes() {
    let d = db().await;
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a"], t0())).await.unwrap().version_id;
    let first = d.mcp.record_probe(&probe(v, "https://a", "t1", t0())).await.unwrap().unwrap();
    let later = t0() + Duration::hours(3);
    let second = d.mcp.record_probe(&probe(v, "https://a", "t1", later)).await.unwrap().unwrap();
    assert_eq!(first, second, "the surface keeps the id approvals hang off");
    let surfaces = d.mcp.surfaces(v).await.unwrap();
    assert_eq!(surfaces[0].captured_at, later);

    for _ in 0..2 {
        let failed = ProbeRun {
            outcome: Err("connection refused".into()),
            ..probe(v, "https://a", "t1", later + Duration::hours(1))
        };
        assert_eq!(d.mcp.record_probe(&failed).await.unwrap(), None);
    }
    assert_eq!(d.mcp.surfaces(v).await.unwrap().len(), 2, "declared and the last good probe");
    assert_eq!(row(&d, d.mirror).await.current.unwrap().id, first);
    let runs = d.mcp.probe_runs(v).await.unwrap();
    assert_eq!((runs.len(), runs[0].ok, runs[0].error.as_deref()), (4, false, Some("connection refused")));
}

#[tokio::test]
async fn two_remotes_with_different_tool_sets_do_not_flip_the_current_surface() {
    let d = db().await;
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a", "https://b"], t0())).await.unwrap().version_id;
    let mut currents = Vec::new();
    for cycle in 0..3 {
        let at = t0() + Duration::hours(cycle);
        let order = if cycle % 2 == 0 { ["https://a", "https://b"] } else { ["https://b", "https://a"] };
        for url in order {
            d.mcp.record_probe(&probe(v, url, &format!("tools-{url}"), at)).await.unwrap();
        }
        currents.push(row(&d, d.mirror).await.current.unwrap().remote_url);
    }
    assert_eq!(currents, vec!["https://a"; 3]);
}

#[tokio::test]
async fn a_stdio_only_version_is_approvable_at_the_declared_slot_and_the_first_probe_asks_again() {
    let d = db().await;
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a"], t0())).await.unwrap().version_id;
    let pending = row(&d, d.mirror).await;
    assert_eq!((pending.surface_endpoints, pending.approved_endpoints), (1, 0));
    d.mcp.decide(&[approval(d.mirror, "", None, t0())]).await.unwrap();
    let approved = row(&d, d.mirror).await;
    assert_eq!((approved.approved_endpoints, approved.worst_drift), (1, Drift::None));
    assert_eq!(approved.decision, Some(Decision::Approved));

    d.mcp.record_probe(&probe(v, "https://a", "t1", t0())).await.unwrap();
    let probed = row(&d, d.mirror).await;
    assert_eq!(probed.surface_endpoints, 1, "the first observation replaces the slot");
    assert_eq!(probed.worst_drift, Drift::Tools, "tools seen for the first time are a review");
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_approvals WHERE remote_url = 'https://a'").await, 1);

    d.mcp.decide(&[approval(d.mirror, "https://a", Some("t1"), t0())]).await.unwrap();
    assert_eq!(row(&d, d.mirror).await.worst_drift, Drift::None);
    d.mcp.record_probe(&probe(v, "https://a", "t2", t0() + Duration::hours(1))).await.unwrap();
    assert_eq!(row(&d, d.mirror).await.worst_drift, Drift::Tools, "a changed tool list invalidates the approval");
}

#[tokio::test]
async fn a_recovered_second_remote_is_a_new_endpoint_and_a_withdrawn_one_leaves_the_quorum() {
    let d = db().await;
    let urls = ["https://a", "https://b"];
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &urls, t0())).await.unwrap().version_id;
    d.mcp.record_probe(&probe(v, "https://a", "ta", t0())).await.unwrap();
    d.mcp.decide(&[approval(d.mirror, "https://a", Some("ta"), t0())]).await.unwrap();
    assert!(row(&d, d.mirror).await.approved_endpoints == 1);

    d.mcp.record_probe(&probe(v, "https://b", "tb", t0() + Duration::hours(1))).await.unwrap();
    let recovered = row(&d, d.mirror).await;
    assert_eq!((recovered.surface_endpoints, recovered.approved_endpoints), (2, 1));
    assert_eq!(recovered.worst_drift, Drift::NewEndpoint);
    assert_eq!(recovered.drifted_remote.as_deref(), Some("https://b"));

    d.mcp.decide(&[approval(d.mirror, "https://b", Some("tb"), t0())]).await.unwrap();
    d.mcp.record_probe(&probe(v, "https://b", "tb2", t0() + Duration::hours(2))).await.unwrap();
    let drifted = row(&d, d.mirror).await;
    assert_eq!((drifted.worst_drift, drifted.drifted_remote.as_deref()), (Drift::Tools, Some("https://b")));

    let shrunk = record(d.mirror, "io.github.acme/x", "1.0.0", "{\"v\":2}", &["https://a"], t0() + Duration::hours(3));
    d.mcp.upsert_record(&shrunk).await.unwrap();
    let after = row(&d, d.mirror).await;
    assert_eq!((after.surface_endpoints, after.approved_endpoints, after.worst_drift), (1, 1, Drift::None));
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_surfaces WHERE remote_url = 'https://b'").await, 0);
    assert_eq!(scalar(&d, "SELECT COUNT(*) FROM mcp_approvals WHERE remote_url = 'https://b'").await, 0);
}

#[tokio::test]
async fn a_group_approval_does_not_leak_to_a_sibling_and_hide_filters_on_the_addressed_verdict() {
    let d = db().await;
    let other = d
        .stores
        .repositories()
        .create(
            &RepoSpec {
                name: "other",
                kind: RepoKind::Hosted,
                format: Format::Mcp,
                visibility: Visibility::Public,
                upstream: None,
                members: &[],
            },
            t0(),
        )
        .await
        .unwrap()
        .id;
    d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &[], t0())).await.unwrap();
    d.mcp.decide(&[approval(d.team, "", None, t0())]).await.unwrap();
    assert_eq!(row(&d, d.team).await.approved_endpoints, 1);
    assert_eq!(row(&d, other).await.approved_endpoints, 0, "falls back to the member, which approved nothing");
    let hidden = |addressed| PageQuery {
        member: d.mirror,
        addressed,
        limit: 10,
        require_approved: true,
        ..PageQuery::default()
    };
    assert_eq!(d.mcp.page(&hidden(d.team)).await.unwrap().len(), 1);
    assert!(d.mcp.page(&hidden(other)).await.unwrap().is_empty());
    assert!(d.mcp.page(&hidden(d.mirror)).await.unwrap().is_empty());
}

#[tokio::test]
async fn filters_page_closed_sets_deny_only_sets_and_star_ranges() {
    let d = db().await;
    for name in ["com.other/a", "io.github.acme/a", "io.github.acme/b", "io.github.acmecorp/c", "io.github.evil/x"] {
        d.mcp.upsert_record(&record(d.mirror, name, "1.0.0", "{}", &[], t0())).await.unwrap();
    }
    let d = &d;
    let names = |filters: Vec<NameFilter>| {
        let q = PageQuery {
            member: d.mirror,
            addressed: d.mirror,
            limit: 100,
            filters,
            ..PageQuery::default()
        };
        async move { d.mcp.page(&q).await.unwrap().into_iter().map(|r| r.name).collect::<Vec<_>>() }
    };
    let rule = |p: &str, e| AllowRule::new(p, e).unwrap();
    let closed = NameFilter { rules: vec![rule("io.github.acme/*", Effect::Allow)] };
    assert_eq!(names(vec![closed]).await, vec!["io.github.acme/a", "io.github.acme/b"]);
    let deny_only = NameFilter { rules: vec![rule("io.github.evil/*", Effect::Deny)] };
    assert_eq!(names(vec![deny_only]).await.len(), 4);
    let reopened = NameFilter {
        rules: vec![rule("io.github.*", Effect::Deny), rule("io.github.acme/a", Effect::Allow)],
    };
    assert!(names(vec![reopened]).await.contains(&"io.github.acme/a".to_string()), "a longer allow reopens a deny");

    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT id FROM mcp_server_versions v
         WHERE v.repository_id = 1 AND (v.name >= 'io.github.acme/' AND v.name < 'io.github.acme0')",
    )
    .fetch_all(&d.stores.pool())
    .await
    .unwrap();
    let detail: String = plan.iter().map(|p| p.3.clone()).collect::<Vec<_>>().join(" | ");
    assert!(detail.contains("INDEX") && detail.contains("name>? AND name<?"), "{detail}");
}

#[tokio::test]
async fn suppression_is_per_repository_and_leaves_the_finding_stored() {
    let d = db().await;
    let v = d.mcp.upsert_record(&record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a"], t0())).await.unwrap().version_id;
    let mut run = probe(v, "https://a", "t", t0());
    if let Ok(s) = &mut run.outcome {
        s.findings = vec![finding("cross_tool", false, "search"), finding("cross_tool", false, "fetch")];
    }
    d.mcp.record_probe(&run).await.unwrap();
    let id = d.mcp.add_suppression(d.team, "cross_tool", "search", Some("admin"), t0()).await.unwrap();
    assert_eq!(row(&d, d.team).await.findings_medium, 1);
    assert_eq!(row(&d, d.mirror).await.findings_medium, 2, "another repository's suppression moves nothing");
    let findings = d.mcp.findings_of(v, d.team).await.unwrap();
    assert_eq!(findings.iter().filter(|f| f.suppressed).count(), 1);
    d.mcp.delete_suppression(d.team, id, t0()).await.unwrap();
    assert_eq!(row(&d, d.team).await.findings_medium, 2);
}

#[tokio::test]
async fn taking_latest_moves_it_and_purge_keeps_hosted_rows() {
    let d = db().await;
    let mut first = record(d.team, "io.github.acme/x", "1.0.0", "{}", &[], t0());
    first.hosted = true;
    first.take_latest = true;
    d.mcp.upsert_record(&first).await.unwrap();
    let mut second = first.clone();
    second.version = "1.1.0".into();
    d.mcp.upsert_record(&second).await.unwrap();
    let latest = d.mcp.version(d.team, d.team, "io.github.acme/x", None).await.unwrap().unwrap();
    assert_eq!(latest.version, "1.1.0");
    assert_eq!(d.mcp.hosted_count(d.team).await.unwrap(), 2);

    d.mcp.upsert_record(&record(d.mirror, "io.github.acme/y", "1.0.0", "{}", &[], t0())).await.unwrap();
    assert_eq!(d.mcp.hosted_count(d.mirror).await.unwrap(), 0);
    assert_eq!(d.mcp.purge(d.mirror).await.unwrap(), 1);
    assert_eq!(d.mcp.purge(d.team).await.unwrap(), 0);
}

#[tokio::test]
async fn denormalised_counters_match_a_recount_after_every_write_path() {
    let d = db().await;
    let mut r = record(d.mirror, "io.github.acme/x", "1.0.0", "{}", &["https://a", "https://b"], t0());
    r.declared.findings = vec![finding("config_path", false, "")];
    let v = d.mcp.upsert_record(&r).await.unwrap().version_id;
    let mut run = probe(v, "https://a", "t", t0());
    if let Ok(s) = &mut run.outcome {
        s.findings = vec![finding("model_directive", true, "q")];
    }
    d.mcp.record_probe(&run).await.unwrap();
    let attested = observed(SurfaceSource::Attested, "", "p1", "ta", vec![finding("cross_tool", false, "q")]);
    d.mcp.record_attested(v, &attested, t0()).await.unwrap();
    d.mcp.decide(&[approval(d.mirror, "https://a", Some("t"), t0())]).await.unwrap();
    let recount = |confidence: &'static str| {
        let sql = format!(
            "SELECT COUNT(*) FROM mcp_findings f JOIN mcp_surfaces s ON f.subject_id = s.id
             WHERE f.subject_kind = 'surface' AND s.version_id = {v} AND f.confidence = '{confidence}'"
        );
        let pool = d.stores.pool();
        async move { sqlx::query_scalar::<_, i64>(&sql).fetch_one(&pool).await.unwrap() }
    };
    let served = row(&d, d.mirror).await;
    assert_eq!((served.findings_high, served.findings_medium), (recount("high").await, recount("medium").await));
    assert_eq!(served.current.unwrap().source, SurfaceSource::Attested);
}
