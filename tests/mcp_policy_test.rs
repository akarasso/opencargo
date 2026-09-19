mod common;

use std::collections::HashMap;

use serde_json::json;

use common::mcp::*;
use common::{group, policy_verdicts, sentinel, spawn_server, verdict_of, wait_for_policy_rows, SpawnOpts, TestServer};
use opencargo::config::{RepositoryFormat, Visibility};
use opencargo::policy::rules::PolicyConfig;

fn rules(keys: &[&str]) -> PolicyConfig {
    let text: String = keys.iter().map(|k| format!("{k} = true\n")).collect();
    toml::from_str(&text).unwrap()
}

async fn served(server: &TestServer, repo: &str, name: &str, version: &str) {
    let (status, body) = get(server, &format!("/{repo}/v0.1/servers/{}/versions/{version}", urlencode(name))).await;
    assert_eq!(status, 200, "{body}");
}

fn url(server: &TestServer, repo: &str, name: &str) -> String {
    format!("{}/{repo}/v0.1/servers/{}/versions/1.0.0", server.base_url, urlencode(name))
}

#[tokio::test]
async fn mcp_detail_records_one_event_and_the_list_records_none() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![mirror("mirror")],
        policy: HashMap::from([("mirror".into(), rules(&["mcp_injection"]))]),
        ..Default::default()
    })
    .await;
    seed(&server, "mirror", &[envelope(record("io.github.acme/x", "1.0.0"), true), envelope(record("io.github.acme/y", "1.0.0"), true)]).await;
    let (status, _) = get(&server, "/mirror/v0.1/servers").await;
    assert_eq!(status, 200);
    let rows = sentinel(&server, &url(&server, "mirror", "io.github.acme/x"), 1).await;
    assert_eq!((rows[0].format.as_str(), rows[0].name.as_str()), ("mcp", "io.github.acme/x"));
    assert_eq!(rows[0].date_source, "registry");
    let verdicts = policy_verdicts(&server).await;
    assert_eq!(
        verdict_of(&verdicts, rows[0].id, "mcp_injection"),
        ("unknown", "tool descriptions not observed (declared only)")
    );
}

#[tokio::test]
async fn a_teams_verdict_is_recorded_against_its_own_repository_not_the_mirrors() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            mirror("mirror"),
            group("team-a", RepositoryFormat::Mcp, &["mirror"]),
            group("team-b", RepositoryFormat::Mcp, &["mirror"]),
        ],
        policy: HashMap::from([("mirror".into(), rules(&["mcp_allowlist", "mcp_drift"]))]),
        mcp: settings(&[("team-a", mode("warn", &["io.github.acme/*"], &[]))]),
        ..Default::default()
    })
    .await;
    seed(&server, "mirror", &[envelope(record("com.other/y", "1.0.0"), true)]).await;
    approve(&server, "team-b", "com.other/y", "1.0.0").await;
    served(&server, "team-a", "com.other/y", "1.0.0").await;
    served(&server, "team-b", "com.other/y", "1.0.0").await;
    let rows = wait_for_policy_rows(&server, 2).await;
    let verdicts = policy_verdicts(&server).await;
    let a = rows.iter().find(|r| r.requested_repo == "team-a").unwrap();
    let b = rows.iter().find(|r| r.requested_repo == "team-b").unwrap();
    assert_eq!(a.member_repo, "mirror");
    assert_eq!(
        verdict_of(&verdicts, a.id, "mcp_allowlist"),
        ("would_block", "com.other/y matches no allow rule of team-a"),
        "mcp_allowlist_rule_would_block_an_unlisted_server, on a warn repository"
    );
    assert_eq!(verdict_of(&verdicts, b.id, "mcp_allowlist").0, "pass");
    assert_eq!(verdict_of(&verdicts, a.id, "mcp_drift"), ("unknown", "never approved"));
    assert_eq!(verdict_of(&verdicts, b.id, "mcp_drift").0, "pass", "team-b's own approval");
}

#[tokio::test]
async fn mcp_transport_would_block_unapproved_stdio() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![mirror("mirror")],
        policy: HashMap::from([("mirror".into(), rules(&["mcp_transport"]))]),
        ..Default::default()
    })
    .await;
    let stdio = json!({"name": "io.github.acme/cli", "description": "d", "version": "1.0.0",
        "packages": [{"registryType": "npm", "identifier": "@acme/cli", "transport": {"type": "stdio"}}]});
    seed(&server, "mirror", &[envelope(stdio, true)]).await;
    let rows = sentinel(&server, &url(&server, "mirror", "io.github.acme/cli"), 1).await;
    let verdicts = policy_verdicts(&server).await;
    assert_eq!(verdict_of(&verdicts, rows[0].id, "mcp_transport"), ("would_block", "stdio package not approved"));
    approve(&server, "mirror", "io.github.acme/cli", "1.0.0").await;
    let rows = sentinel(&server, &url(&server, "mirror", "io.github.acme/cli"), 2).await;
    let verdicts = policy_verdicts(&server).await;
    assert_eq!(verdict_of(&verdicts, rows[1].id, "mcp_transport").0, "pass", "a_stdio_only_version_is_approvable_and_stops_would_blocking");
}

#[tokio::test]
async fn mcp_drift_would_block_after_a_declared_permission_change() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![mirror("mirror")],
        policy: HashMap::from([("mirror".into(), rules(&["mcp_drift"]))]),
        ..Default::default()
    })
    .await;
    seed(&server, "mirror", &[envelope(record("io.github.acme/x", "1.0.0"), true)]).await;
    approve(&server, "mirror", "io.github.acme/x", "1.0.0").await;
    let mut moved = record("io.github.acme/x", "1.0.0");
    moved["remotes"][0]["headers"] = json!([{"name": "X-Tenant", "value": "attacker"}]);
    seed(&server, "mirror", &[envelope(moved, true)]).await;
    let rows = sentinel(&server, &url(&server, "mirror", "io.github.acme/x"), 1).await;
    let verdicts = policy_verdicts(&server).await;
    let (verdict, reason) = verdict_of(&verdicts, rows[0].id, "mcp_drift");
    assert_eq!(verdict, "would_block");
    assert!(reason.starts_with("declared permissions changed"), "{reason}");
}

#[tokio::test]
async fn hosted_mcp_member_records_nothing() {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("internal", Visibility::Public),
            mirror("mirror"),
            group("all", RepositoryFormat::Mcp, &["internal", "mirror"]),
        ],
        policy: HashMap::from([("mirror".into(), rules(&["mcp_injection"]))]),
        ..Default::default()
    })
    .await;
    seed(&server, "internal", &[envelope(record("io.github.acme/internal", "1.0.0"), true)]).await;
    seed(&server, "mirror", &[envelope(record("io.github.acme/x", "1.0.0"), true)]).await;
    served(&server, "all", "io.github.acme/internal", "1.0.0").await;
    let rows = sentinel(&server, &url(&server, "all", "io.github.acme/x"), 1).await;
    assert_eq!(rows[0].name, "io.github.acme/x");
}
