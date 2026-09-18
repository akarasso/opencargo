mod common;

use common::mcp::*;
use common::{group, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};
use serde_json::{json, Value};

fn opts(repositories: Vec<opencargo::config::RepositoryConfig>) -> SpawnOpts {
    SpawnOpts {
        repositories,
        ..Default::default()
    }
}

#[tokio::test]
async fn list_pages_with_the_registry_cursor_shape() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    seed_many(&server, "mcp", (0..5).map(|i| format!("io.github.acme/s{i}"))).await;
    let (status, page) = get(&server, "/mcp/v0.1/servers?limit=2").await;
    assert_eq!(status, 200);
    assert_eq!(names(&page), vec!["io.github.acme/s0@1.0.0", "io.github.acme/s1@1.0.0"]);
    assert_eq!(page["metadata"]["count"], 2);
    assert_eq!(page["metadata"]["nextCursor"], "io.github.acme/s1:1.0.0");
    assert_eq!(walk(&server, "mcp", 2, "").await.len(), 5);
    let (_, clamped) = get(&server, "/mcp/v0.1/servers?limit=1000").await;
    assert_eq!(names(&clamped).len(), 5);
    assert!(clamped["metadata"].get("nextCursor").is_none());
}

#[tokio::test]
async fn group_merges_members_and_dedupes_by_name_version() {
    let server = spawn_server(opts(vec![
        mirror("a"),
        mirror("b"),
        group("all", RepositoryFormat::Mcp, &["a", "b"]),
    ]))
    .await;
    seed(&server, "a", &[envelope(record("io.github.acme/x", "1.0.0"), true)]).await;
    seed(
        &server,
        "b",
        &[envelope(record("io.github.acme/x", "1.0.0"), true), envelope(record("com.other/y", "2.0.0"), true)],
    )
    .await;
    let (_, page) = get(&server, "/all/v0.1/servers").await;
    assert_eq!(names(&page), vec!["com.other/y@2.0.0", "io.github.acme/x@1.0.0"]);
    let x = &page["servers"][1];
    assert_eq!(x["_meta"][MIRROR]["repository"], "all");
}

#[tokio::test]
async fn private_member_is_skipped_for_a_reader_without_grant() {
    let mut secret = mirror("secret");
    secret.visibility = Visibility::Private;
    let server = spawn_server(opts(vec![mirror("open"), secret, group("all", RepositoryFormat::Mcp, &["open", "secret"])])).await;
    seed(&server, "open", &[envelope(record("com.open/x", "1"), true)]).await;
    seed(&server, "secret", &[envelope(record("com.secret/x", "1"), true)]).await;
    let (_, page) = get(&server, "/all/v0.1/servers").await;
    assert_eq!(names(&page), vec!["com.open/x@1"]);
    let (status, _) = get(&server, "/all/v0.1/servers/com.secret%2Fx/versions/1").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_private_mcp_repo_answers_401_on_the_catalog() {
    let mut secret = mirror("secret");
    secret.visibility = Visibility::Private;
    let server = spawn_server(opts(vec![secret])).await;
    let (status, _) = get(&server, "/secret/v0.1/servers").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn detail_latest_resolves_is_latest_and_encoded_server_name_reaches_the_handler() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    seed(
        &server,
        "mcp",
        &[envelope(record("io.github.acme/x", "1.0.0"), false), envelope(record("io.github.acme/x", "1.1.0"), true)],
    )
    .await;
    let (status, latest) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fx/versions/latest").await;
    assert_eq!(status, 200, "{latest}");
    assert_eq!(latest["server"]["version"], "1.1.0");
    let (status, exact) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fx/versions/1.0.0").await;
    assert_eq!(status, 200);
    assert_eq!(exact["server"]["version"], "1.0.0");
    let (_, versions) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fx/versions").await;
    assert_eq!(names(&versions).len(), 2);
    let (status, _) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fnope/versions/latest").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn non_mcp_repository_answers_400_on_v0_1_routes() {
    let server = spawn_server(opts(vec![common::hosted("npm", RepositoryFormat::Npm, Visibility::Public)])).await;
    for path in ["/npm/v0.1/servers", "/npm/v0.1/servers/a%2Fb/versions", "/npm/v0/servers/a%2Fb/versions/1"] {
        let (status, _) = get(&server, path).await;
        assert_eq!(status, 400, "{path}");
    }
}

#[tokio::test]
async fn v0_alias_and_v0_1_serve_the_same_bytes_and_a_doubled_base_collapses() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    seed_many(&server, "mcp", (0..3).map(|i| format!("io.github.acme/s{i}"))).await;
    let (_, canonical) = get(&server, "/mcp/v0.1/servers").await;
    for alias in ["/mcp/v0/servers", "/mcp/v0/servers/v0.1/servers", "/mcp/v0.1/v0.1/servers"] {
        let (status, page) = get(&server, alias).await;
        assert_eq!(status, 200, "{alias}");
        assert_eq!(names(&page), names(&canonical), "{alias}");
    }
    let (status, detail) = get(&server, "/mcp/v0/servers/v0.1/servers/io.github.acme%2Fs1/versions/1.0.0").await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["server"]["name"], "io.github.acme/s1");
}

#[tokio::test]
async fn warn_mode_flags_in_our_own_meta_and_leaves_the_official_block_byte_identical() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    let mut rec = record("io.github.acme/x", "1.0.0");
    rec["_meta"] = json!({"com.acme/publisher": {"team": "platform"}});
    let mut env = envelope(rec, true);
    env["_meta"][MIRROR] = json!({"approval": "approved", "forged": true});
    seed(&server, "mcp", &[env.clone()]).await;
    let (_, page) = get(&server, "/mcp/v0.1/servers").await;
    let served = &page["servers"][0];
    assert_eq!(served["_meta"][OFFICIAL], env["_meta"][OFFICIAL]);
    assert_eq!(served["server"], env["server"]);
    let ours = &served["_meta"][MIRROR];
    assert_eq!(ours["approval"], "pending", "an inbound mirror key never reaches a client");
    assert!(ours.get("forged").is_none());
    assert_eq!(ours["gate"], "flagged");
    assert_eq!(ours["reason"], "not approved");
    assert_eq!(ours["toolsSource"], "declared");
    assert_eq!(ours["findings"], json!({"high": 0, "medium": 0}));
    assert_eq!(served["_meta"][OFFICIAL]["status"], "active", "a fresh mirror serves nothing as deprecated");
}

#[tokio::test]
async fn hide_mode_404s_an_unapproved_version_and_serves_it_once_approved() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[("mcp", mode("hide", &[], &[]))]),
        ..opts(vec![mirror("mcp")])
    })
    .await;
    seed(&server, "mcp", &[envelope(record("io.github.acme/x", "1.0.0"), true)]).await;
    let (status, _) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fx/versions/1.0.0").await;
    assert_eq!(status, 404);
    let (_, page) = get(&server, "/mcp/v0.1/servers").await;
    assert!(names(&page).is_empty());
    approve(&server, "mcp", "io.github.acme/x", "1.0.0").await;
    let (status, served) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fx/versions/1.0.0").await;
    assert_eq!(status, 200);
    assert_eq!(served["_meta"][MIRROR]["approval"], "approved");
    assert_eq!(served["_meta"][MIRROR]["gate"], "serve");
}

#[tokio::test]
async fn allowlist_closed_set_hides_unmatched_and_deny_only_pages_the_rest() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[
            ("closed", mode("hide", &["io.github.acme/*"], &[])),
            ("denying", mode("hide", &[], &["io.github.evil/*"])),
            ("warned", mode("warn", &["io.github.acme/*"], &[])),
        ]),
        ..opts(vec![mirror("closed"), mirror("denying"), mirror("warned")])
    })
    .await;
    let all = ["io.github.acme/a", "io.github.evil/b", "com.other/c"];
    for repo in ["closed", "denying", "warned"] {
        seed_many(&server, repo, all.iter().map(|s| s.to_string())).await;
        for name in all {
            approve(&server, repo, name, "1.0.0").await;
        }
    }
    let (_, closed) = get(&server, "/closed/v0.1/servers").await;
    assert_eq!(names(&closed), vec!["io.github.acme/a@1.0.0"]);
    let (_, denying) = get(&server, "/denying/v0.1/servers").await;
    assert_eq!(names(&denying), vec!["com.other/c@1.0.0", "io.github.acme/a@1.0.0"]);
    let (_, warned) = get(&server, "/warned/v0.1/servers").await;
    assert_eq!(names(&warned).len(), 3);
    let flagged = warned["servers"].as_array().unwrap().iter().find(|s| s["server"]["name"] == "com.other/c").unwrap();
    assert_eq!(flagged["_meta"][MIRROR]["reason"], "matches no allow rule");
}

#[tokio::test]
async fn a_group_narrows_its_member_and_a_member_floor_survives_the_group() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[
            ("team", mode("hide", &["io.github.acme/*"], &[])),
            ("closed-mirror", mode("hide", &["io.github.acme/*"], &[])),
        ]),
        ..opts(vec![
            mirror("open-mirror"),
            mirror("closed-mirror"),
            group("team", RepositoryFormat::Mcp, &["open-mirror"]),
            group("bare", RepositoryFormat::Mcp, &["closed-mirror"]),
        ])
    })
    .await;
    for repo in ["open-mirror", "closed-mirror"] {
        seed_many(&server, repo, ["io.github.acme/a", "com.other/c"].map(String::from)).await;
    }
    for (repo, name) in [("team", "io.github.acme/a"), ("team", "com.other/c"), ("bare", "io.github.acme/a"), ("bare", "com.other/c")] {
        approve(&server, repo, name, "1.0.0").await;
    }
    let (_, team) = get(&server, "/team/v0.1/servers").await;
    assert_eq!(names(&team), vec!["io.github.acme/a@1.0.0"], "group_allowlist_hides_a_row_the_member_repo_allows");
    let (_, bare) = get(&server, "/bare/v0.1/servers").await;
    assert_eq!(names(&bare), vec!["io.github.acme/a@1.0.0"], "member_allowlist_still_hides_a_row_the_group_allows");
    let (status, _) = get(&server, "/bare/v0.1/servers/com.other%2Fc/versions/1.0.0").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn an_intermediate_groups_allowlist_still_hides_a_row_the_outer_group_allows() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[("platform-shared", mode("hide", &["io.github.acme/*"], &[]))]),
        ..opts(vec![
            mirror("mirror"),
            group("platform-shared", RepositoryFormat::Mcp, &["mirror"]),
            group("team-a", RepositoryFormat::Mcp, &["platform-shared"]),
        ])
    })
    .await;
    seed_many(&server, "mirror", ["io.github.acme/a", "com.other/c"].map(String::from)).await;
    let (_, page) = get(&server, "/team-a/v0.1/servers").await;
    assert_eq!(names(&page), vec!["io.github.acme/a@1.0.0"]);
}

#[tokio::test]
async fn a_leaf_reachable_by_two_paths_keeps_the_stricter_one() {
    for members in [["mirror", "platform-shared"], ["platform-shared", "mirror"]] {
        let server = spawn_server(SpawnOpts {
            mcp: settings(&[("platform-shared", mode("hide", &["io.github.acme/*"], &[]))]),
            ..opts(vec![
                mirror("mirror"),
                group("platform-shared", RepositoryFormat::Mcp, &["mirror"]),
                group("team-a", RepositoryFormat::Mcp, &members),
            ])
        })
        .await;
        seed_many(&server, "mirror", ["io.github.acme/a", "com.other/c"].map(String::from)).await;
        let (_, page) = get(&server, "/team-a/v0.1/servers").await;
        assert_eq!(names(&page), vec!["io.github.acme/a@1.0.0"], "{members:?}");
    }
}

#[tokio::test]
async fn two_groups_over_one_mirror_have_independent_allowlists_and_approvals() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[
            ("team-a", mode("hide", &["io.github.acme/*"], &[])),
            ("team-b", mode("hide", &[], &[])),
        ]),
        ..opts(vec![
            mirror("mirror"),
            group("team-a", RepositoryFormat::Mcp, &["mirror"]),
            group("team-b", RepositoryFormat::Mcp, &["mirror"]),
        ])
    })
    .await;
    seed_many(&server, "mirror", ["io.github.acme/x", "com.other/y"].map(String::from)).await;
    approve(&server, "team-a", "io.github.acme/x", "1.0.0").await;
    approve(&server, "team-a", "com.other/y", "1.0.0").await;
    approve(&server, "team-b", "com.other/y", "1.0.0").await;
    let (_, a) = get(&server, "/team-a/v0.1/servers").await;
    assert_eq!(names(&a), vec!["io.github.acme/x@1.0.0"]);
    let (_, b) = get(&server, "/team-b/v0.1/servers").await;
    assert_eq!(names(&b), vec!["com.other/y@1.0.0"], "a_group_approval_does_not_leak_to_a_sibling_group_under_hide");
    let (status, _) = get(&server, "/team-b/v0.1/servers/io.github.acme%2Fx/versions/1.0.0").await;
    assert_eq!(status, 404);
    let (_, mirror_page) = get(&server, "/mirror/v0.1/servers").await;
    assert_eq!(names(&mirror_page).len(), 2, "the mirror itself stays in warn");
}

#[tokio::test]
async fn hide_mode_with_a_closed_allowlist_pages_a_thousand_row_mirror_exactly_once() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[("mcp", mode("hide", &["io.github.zz/*"], &[]))]),
        ..opts(vec![mirror("mcp")])
    })
    .await;
    let mut all: Vec<String> = (0..960).map(|i| format!("com.noise/s{i:04}")).collect();
    let allowed: Vec<String> = (0..40).map(|i| format!("io.github.zz/s{i:02}")).collect();
    all.extend(allowed.clone());
    seed_many(&server, "mcp", all.clone()).await;
    let approved: Vec<String> = all.iter().step_by(3).chain(&allowed).cloned().collect();
    approve_all(&server, "mcp", &approved, "1.0.0").await;
    let (_, first) = get(&server, "/mcp/v0.1/servers?limit=30").await;
    assert_eq!(names(&first).len(), 30, "the page fills from rows past the hidden ones");
    let served = walk(&server, "mcp", 30, "").await;
    let want: Vec<String> = allowed.iter().map(|n| format!("{n}@1.0.0")).collect();
    assert_eq!(served, want);

    let warned = spawn_server(opts(vec![mirror("mcp")])).await;
    seed_many(&warned, "mcp", all.clone()).await;
    let served = walk(&warned, "mcp", 7, "").await;
    let mut want: Vec<String> = all.iter().map(|n| format!("{n}@1.0.0")).collect();
    want.sort();
    assert_eq!(served, want, "every row exactly once, in order");
}

#[tokio::test]
async fn a_page_whose_batch_is_entirely_hidden_still_emits_a_cursor() {
    let server = spawn_server(SpawnOpts {
        mcp: settings(&[("mcp", mode("hide", &["com.*", "com.noise0/zkeep"], &["com.noise0/*"]))]),
        ..opts(vec![mirror("mcp")])
    })
    .await;
    let mut all: Vec<String> = (0..2500).map(|i| format!("com.noise0/s{i:04}")).collect();
    all.push("com.noise0/zkeep".into());
    seed_many(&server, "mcp", all.clone()).await;
    approve_all(&server, "mcp", &all, "1.0.0").await;
    let (_, first) = get(&server, "/mcp/v0.1/servers?limit=30").await;
    assert!(names(&first).is_empty(), "the deny rule a longer allow reopens is judged in memory");
    assert!(first["metadata"]["nextCursor"].is_string(), "{}", first["metadata"]);
    assert_eq!(walk(&server, "mcp", 30, "").await, vec!["com.noise0/zkeep@1.0.0"]);
}

#[tokio::test]
async fn paging_a_group_of_three_exhausted_members_yields_every_row_exactly_once() {
    let server = spawn_server(opts(vec![
        mirror("m0"),
        mirror("m1"),
        mirror("m2"),
        group("all", RepositoryFormat::Mcp, &["m0", "m1", "m2"]),
    ]))
    .await;
    let mut want = Vec::new();
    for m in 0..3 {
        let names: Vec<String> = (0..25).map(|i| format!("io.m{m}/s{i:02}")).collect();
        want.extend(names.iter().map(|n| format!("{n}@1.0.0")));
        seed_many(&server, &format!("m{m}"), names).await;
    }
    want.sort();
    assert_eq!(walk(&server, "all", 30, "").await, want);
}

#[tokio::test]
async fn deleted_status_is_kept_and_hidden_unless_include_deleted() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    let mut gone = envelope(record("io.github.acme/gone", "1.0.0"), true);
    gone["_meta"][OFFICIAL]["status"] = json!("deleted");
    seed(&server, "mcp", &[gone, envelope(record("io.github.acme/kept", "1.0.0"), true)]).await;
    let (_, page) = get(&server, "/mcp/v0.1/servers").await;
    assert_eq!(names(&page), vec!["io.github.acme/kept@1.0.0"]);
    let (_, all) = get(&server, "/mcp/v0.1/servers?include_deleted=true").await;
    assert_eq!(names(&all).len(), 2);
    let (_, since) = get(&server, "/mcp/v0.1/servers?updated_since=2000-01-01T00:00:00Z").await;
    assert_eq!(names(&since).len(), 2, "updated_since implies include_deleted");
    let (_, explicit) = get(&server, "/mcp/v0.1/servers?updated_since=2000-01-01T00:00:00Z&include_deleted=false").await;
    assert_eq!(names(&explicit).len(), 1);
    let (status, detail) = get(&server, "/mcp/v0.1/servers/io.github.acme%2Fgone/versions/1.0.0").await;
    assert_eq!(status, 200);
    assert_eq!(detail["_meta"][MIRROR]["reason"], "deleted upstream");
}

#[tokio::test]
async fn an_options_preflight_on_the_catalog_answers_with_cors_headers_and_no_token() {
    let server = spawn_server(opts(vec![mirror("mcp")])).await;
    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{}/mcp/v0.1/servers", server.base_url))
        .header("Origin", "vscode-file://vscode-app")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(resp.headers()["access-control-allow-origin"], "*");
    let methods = resp.headers()["access-control-allow-methods"].to_str().unwrap().to_ascii_uppercase();
    assert!(methods.contains("GET") && methods.contains("OPTIONS"), "{methods}");
    let resp = client
        .get(format!("{}/mcp/v0.1/servers", server.base_url))
        .header("Origin", "vscode-file://vscode-app")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["access-control-allow-origin"], "*", "routes::a_catalog_get_carries_cors_headers");
    let body: Value = resp.json().await.unwrap();
    assert!(body["servers"].is_array());
}
