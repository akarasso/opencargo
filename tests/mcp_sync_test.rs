mod common;

use chrono::{Duration, Utc};
use serde_json::{json, Value};

use common::fake_upstream::mcp::{self as upstream, server, FakeRegistry};
use common::mcp::*;
use common::{spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};

async fn mirrored(fake: &FakeRegistry) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![common::proxy("mirror", MCP, &fake.base_url)],
        ..Default::default()
    })
    .await
}

async fn sync(server: &TestServer, repo: &str, full: bool) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/mcp/{repo}/sync", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({"full": full}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn full_sync_pages_until_cursor_empty_and_sends_the_flag_explicitly() {
    let fake = upstream::start().await;
    for i in 0..250 {
        fake.put(server(&format!("io.github.acme/s{i:03}"), "1.0.0"), "active", Utc::now());
    }
    let srv = mirrored(&fake).await;
    let (status, body) = sync(&srv, "mirror", false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["report"]["pages"], 3);
    assert_eq!(body["report"]["upserted"], 250);
    assert_eq!(body["report"]["full"], true);
    assert_eq!(walk(&srv, "mirror", 100, "").await.len(), 250);
    for q in fake.queries() {
        assert_eq!(q.get("limit").map(String::as_str), Some("100"));
        assert_eq!(q.get("include_deleted").map(String::as_str), Some("true"));
    }
}

#[tokio::test]
async fn incremental_sync_anchors_updated_since_on_the_run_start_not_on_max_updated_at() {
    let fake = upstream::start().await;
    let future = Utc::now() + Duration::days(365);
    fake.put(server("io.github.acme/x", "1.0.0"), "active", future);
    let srv = mirrored(&fake).await;
    let before = Utc::now();
    sync(&srv, "mirror", false).await;
    let (_, body) = sync(&srv, "mirror", false).await;
    assert_eq!(body["report"]["full"], false);
    let since = fake.queries().last().unwrap()["updated_since"].clone();
    let since = chrono::DateTime::parse_from_rfc3339(&since).unwrap().with_timezone(&Utc);
    assert!(since < before, "anchored before the run started, not on {future}");
    assert!(since > before - Duration::minutes(2));
}

#[tokio::test]
async fn a_record_updated_behind_the_cursor_mid_run_is_caught_by_the_next_run() {
    let fake = upstream::start().await;
    let old = Utc::now() - Duration::days(30);
    for i in 0..150 {
        fake.put(server(&format!("io.github.acme/s{i:03}"), "1.0.0"), "active", old);
    }
    fake.state.lock().unwrap().behind_cursor = Some((
        "io.github.acme/s000".into(),
        "1.0.0".into(),
        "rewritten while paging".into(),
        Utc::now(),
    ));
    let srv = mirrored(&fake).await;
    sync(&srv, "mirror", false).await;
    let (_, stale) = get(&srv, "/mirror/v0.1/servers/io.github.acme%2Fs000/versions/1.0.0").await;
    assert_eq!(stale["server"]["description"], "io.github.acme/s000 does things");
    let (_, body) = sync(&srv, "mirror", false).await;
    assert_eq!(body["report"]["changed"], 1, "{body}");
    let (_, fresh) = get(&srv, "/mirror/v0.1/servers/io.github.acme%2Fs000/versions/1.0.0").await;
    assert_eq!(fresh["server"]["description"], "rewritten while paging");
}

#[tokio::test]
async fn a_deleted_upstream_record_is_kept_and_hidden_unless_include_deleted() {
    let fake = upstream::start().await;
    fake.put(server("io.github.acme/x", "1.0.0"), "active", Utc::now() - Duration::days(1));
    let srv = mirrored(&fake).await;
    sync(&srv, "mirror", false).await;
    fake.put(server("io.github.acme/x", "1.0.0"), "deleted", Utc::now());
    let (_, body) = sync(&srv, "mirror", false).await;
    assert_eq!(body["report"]["changed"], 1);
    let (_, page) = get(&srv, "/mirror/v0.1/servers").await;
    assert!(names(&page).is_empty());
    let (_, all) = get(&srv, "/mirror/v0.1/servers?include_deleted=true").await;
    assert_eq!(all["servers"][0]["_meta"][OFFICIAL]["status"], "deleted");
}

#[tokio::test]
async fn a_record_with_a_malformed_name_is_skipped_and_the_page_still_ingests() {
    let fake = upstream::start().await;
    fake.put(server("io.github.acme/good", "1.0.0"), "active", Utc::now());
    fake.put(server("no-namespace", "1.0.0"), "active", Utc::now());
    fake.put(server("io.github.acme/empty", ""), "active", Utc::now());
    let srv = mirrored(&fake).await;
    let (status, body) = sync(&srv, "mirror", false).await;
    assert_eq!(status, 200);
    assert_eq!(body["report"]["skipped"], 2);
    assert_eq!(names(&get(&srv, "/mirror/v0.1/servers").await.1), vec!["io.github.acme/good@1.0.0"]);
}

#[tokio::test]
async fn upstream_failure_keeps_serving_and_the_high_water_does_not_move() {
    let fake = upstream::start().await;
    fake.put(server("io.github.acme/x", "1.0.0"), "active", Utc::now());
    let srv = mirrored(&fake).await;
    sync(&srv, "mirror", false).await;
    let first_since = {
        sync(&srv, "mirror", false).await;
        fake.queries().last().unwrap()["updated_since"].clone()
    };
    fake.set_unavailable(true);
    let (status, _) = sync(&srv, "mirror", false).await;
    assert_eq!(status, 502);
    assert_eq!(names(&get(&srv, "/mirror/v0.1/servers").await.1).len(), 1, "the last good rows are served");
    let (mcp, _) = store(&srv).await;
    let state = mcp.sync_state(repo_id(&srv, "mirror").await).await.unwrap();
    assert_eq!(state.consecutive_failures, 1);
    assert!(state.last_error.unwrap().contains("503"));
    fake.set_unavailable(false);
    sync(&srv, "mirror", false).await;
    let queries = fake.queries();
    let after_failure = &queries[queries.len() - 1]["updated_since"];
    assert!(after_failure >= &first_since, "the failed run did not move the mark past what it never read");
}

#[tokio::test]
async fn a_mirror_created_at_runtime_syncs_on_demand_and_a_hosted_one_has_no_sync() {
    let fake = upstream::start().await;
    fake.put(server("io.github.acme/x", "1.0.0"), "active", Utc::now());
    let srv = spawn_server(SpawnOpts {
        repositories: vec![hosted("internal", opencargo::config::Visibility::Private)],
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/api/v1/repositories", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({"name": "late", "type": "proxy", "format": "mcp", "upstream": fake.base_url, "visibility": "public"}))
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success(), "{}", created.text().await.unwrap());
    let (status, body) = sync(&srv, "late", false).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(names(&get(&srv, "/late/v0.1/servers").await.1), vec!["io.github.acme/x@1.0.0"]);
    let (status, _) = sync(&srv, "internal", false).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn deleting_a_mirror_stops_its_sync_task_and_takes_its_rows() {
    let fake = upstream::start().await;
    fake.put(server("io.github.acme/x", "1.0.0"), "active", Utc::now());
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = opencargo::config::Config {
        repositories: vec![common::proxy("mirror", MCP, &fake.base_url)],
        ..Default::default()
    };
    config.server.storage_path = tmp.path().join("storage").display().to_string();
    config.database.url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let state = common::build_state(&mut config).await.unwrap();
    let supervisor = state.sync_supervisor().unwrap();
    supervisor.reconcile().await.unwrap();
    let mirror = state.repos.by_name("mirror").await.unwrap().unwrap();
    assert_eq!(state.mcp_sync.live(), vec![mirror.id]);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while state.mcp.version(mirror.id, mirror.id, "io.github.acme/x", None).await.unwrap().is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the child never synced");
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    state.repos.retire("mirror", Utc::now()).await.unwrap();
    supervisor.reconcile().await.unwrap();
    assert!(state.mcp_sync.live().is_empty());
    assert!(state.mcp.version(mirror.id, mirror.id, "io.github.acme/x", None).await.unwrap().is_none());
    assert_eq!(state.mcp.sync_state(mirror.id).await.unwrap(), Default::default());
}
