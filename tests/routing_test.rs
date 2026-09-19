//! Routing rules over HTTP: what a client asks for, what the upstream is
//! asked for, and what an administrator can see before activating a rule.
//!
//! "Zero request to the upstream" is the assertion this whole feature exists
//! for, and it is made against a recording tap in front of a second server —
//! not against a mock the code under test could have been written around.

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::upstream_tap::{self, Tap};
use common::{
    build_npm_publish_body, build_tarball, group, hosted, proxy, spawn_server, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const UPSTREAM_REPO: &str = "npm-public";
const PINNED: &str = "@acme/widget";
const FREE: &str = "left-pad";

/// A second opencargo standing in for npmjs, fronted by a recording tap.
struct Upstream {
    _server: TestServer,
    tap: Tap,
}

impl Upstream {
    fn url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}", self.tap.base_url)
    }

    /// Every request the upstream was asked for, whatever the path.
    fn hits(&self) -> Vec<String> {
        self.tap
            .hits
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path)| path.clone())
            .collect()
    }
}

async fn publish(server: &TestServer, repo: &str, name: &str, description: &str) {
    let tarball = build_tarball(&format!(
        r#"{{"name":"{name}","version":"1.0.0","description":"{description}"}}"#
    ));
    let body = build_npm_publish_body(name, "1.0.0", description, &tarball);
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{name}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish {name} to {repo}");
}

/// The impostor: the public upstream holds the very name the group pins
/// internally, plus one ordinary package.
async fn seed_upstream() -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(UPSTREAM_REPO, RepositoryFormat::Npm, Visibility::Public)],
        ..Default::default()
    })
    .await;
    publish(&server, UPSTREAM_REPO, PINNED, "the impostor").await;
    publish(&server, UPSTREAM_REPO, FREE, "an ordinary package").await;
    let tap = upstream_tap::start(&server.base_url).await;
    Upstream { _server: server, tap }
}

/// `all` = hosted `internal`, then `public` in front of the upstream.
async fn spawn_group(up: &Upstream) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("internal", RepositoryFormat::Npm, Visibility::Public),
            hosted("sandbox", RepositoryFormat::Npm, Visibility::Public),
            proxy("public", RepositoryFormat::Npm, &up.url()),
            group("all", RepositoryFormat::Npm, &["internal", "public"]),
        ],
        ..Default::default()
    })
    .await
}

async fn post(server: &TestServer, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("request failed")
}

async fn create_rule(server: &TestServer, body: Value) -> Value {
    let resp = post(server, "/api/v1/routing-rules", body).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "{:?}", resp.text().await);
    resp.json().await.expect("invalid json")
}

/// The rule the whole feature is for: `@acme/*` is served by `internal` and
/// by nothing else.
fn pinned_to_internal() -> Value {
    json!({
        "name": "acme-internal",
        "format": "npm",
        "patterns": ["@acme/*"],
        "effect": "allow_members",
        "targets": ["internal"],
    })
}

async fn status_of(server: &TestServer, path: &str) -> StatusCode {
    reqwest::get(format!("{}{path}", server.base_url))
        .await
        .expect("request failed")
        .status()
}

/// T1, T2, T3: the pinned name is answered by the hosted member or by nobody,
/// and the upstream is never asked for it; a name outside the patterns is
/// untouched.
#[tokio::test]
async fn a_pinned_scope_never_reaches_the_upstream() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;

    assert_eq!(
        status_of(&server, &format!("/all/{PINNED}")).await,
        StatusCode::NOT_FOUND,
        "absent from the hosted member is a 404, never the impostor"
    );
    assert!(
        up.hits().is_empty(),
        "the upstream was asked for something: {:?}",
        up.hits()
    );

    publish(&server, "internal", PINNED, "ours").await;
    let served: Value = reqwest::get(format!("{}/all/{PINNED}", server.base_url))
        .await
        .expect("request failed")
        .json()
        .await
        .expect("invalid json");
    assert_eq!(served["versions"]["1.0.0"]["description"], "ours");
    assert!(up.hits().is_empty(), "still nothing: {:?}", up.hits());

    assert_eq!(status_of(&server, &format!("/all/{FREE}")).await, StatusCode::OK);
    assert!(!up.hits().is_empty(), "a name no rule covers resolves as it always did");
}

/// T4: the spelling the feature exists for. A capital letter reaches the npm
/// read path, and a rule keyed on `normalize` — which is the identity for npm
/// — would have let it walk straight out to the public registry.
#[tokio::test]
async fn a_capital_letter_does_not_walk_around_the_rule() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;

    for spelling in ["@ACME/widget", "@Acme/Widget", "@acme/Widget"] {
        assert_eq!(
            status_of(&server, &format!("/all/{spelling}")).await,
            StatusCode::NOT_FOUND,
            "{spelling}"
        );
    }
    assert!(up.hits().is_empty(), "{:?}", up.hits());
}

/// T7: the proxy's own URL is not a way around the rule. A rule is total over
/// its format, so a repository addressed directly is decided like a member.
#[tokio::test]
async fn the_proxys_own_url_is_not_a_way_around() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    assert_eq!(
        status_of(&server, &format!("/public/{PINNED}")).await,
        StatusCode::OK,
        "without a rule the proxy serves it"
    );

    create_rule(&server, pinned_to_internal()).await;
    assert_eq!(
        status_of(&server, &format!("/public/{PINNED}")).await,
        StatusCode::NOT_FOUND
    );
    let before = up.hits().len();
    assert_eq!(status_of(&server, &format!("/public/{PINNED}")).await, StatusCode::NOT_FOUND);
    assert_eq!(up.hits().len(), before, "no request, and no new cache entry");
}

/// T8: an answer cached before the rule becomes unreachable, by the group and
/// by the proxy's own URL.
#[tokio::test]
async fn an_entry_cached_before_the_rule_is_never_served_again() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    assert_eq!(status_of(&server, &format!("/all/{PINNED}")).await, StatusCode::OK);
    assert!(!up.hits().is_empty(), "it went upstream and was cached");

    create_rule(&server, pinned_to_internal()).await;
    assert_eq!(status_of(&server, &format!("/all/{PINNED}")).await, StatusCode::NOT_FOUND);
    assert_eq!(status_of(&server, &format!("/public/{PINNED}")).await, StatusCode::NOT_FOUND);
}

/// T11: `explain` shows both keys, every rule that refuses, and the snapshot
/// it decided on; a candidate rule is evaluated without being stored.
#[tokio::test]
async fn explain_shows_both_keys_and_every_refusing_rule() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;
    create_rule(
        &server,
        json!({
            "name": "acme-deny",
            "format": "npm",
            "patterns": ["@acme/*"],
            "effect": "deny",
        }),
    )
    .await;

    let explained: Value = post(
        &server,
        "/api/v1/routing-rules/explain",
        json!({ "repository": "all", "name": "@ACME/widget" }),
    )
    .await
    .json()
    .await
    .expect("invalid json");

    assert_eq!(explained["match_key"], "@acme/widget");
    assert_eq!(explained["ident_key"], "@ACME/widget", "npm identity is the spelling");
    assert!(explained["snapshot_version"].as_u64().unwrap() > 0);
    let members = explained["members"].as_array().expect("a member list");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["name"], "internal");
    assert_eq!(members[0]["refused_by"], json!(["acme-deny"]));
    assert_eq!(members[1]["name"], "public");
    assert_eq!(
        members[1]["refused_by"],
        json!(["acme-deny", "acme-internal"]),
        "both, in name order: deleting one would not lift the refusal"
    );

    let candidate: Value = post(
        &server,
        "/api/v1/routing-rules/explain",
        json!({
            "repository": "all",
            "name": FREE,
            "candidate": {
                "name": "try-me",
                "format": "npm",
                "patterns": ["left-*"],
                "effect": "allow_hosted",
            },
        }),
    )
    .await
    .json()
    .await
    .expect("invalid json");
    assert_eq!(candidate["members"][1]["refused_by"], json!(["try-me"]));

    let listed: Value = reqwest::Client::new()
        .get(format!("{}/api/v1/routing-rules", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("request failed")
        .json()
        .await
        .expect("invalid json");
    let names: Vec<&str> = listed["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme-deny", "acme-internal"], "the candidate was not stored");
}

/// T21: a rule is total over its format, so a request that tries to narrow it
/// is refused rather than served with the field quietly dropped.
#[tokio::test]
async fn a_request_that_narrows_a_rule_is_refused() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    let resp = post(
        &server,
        "/api/v1/routing-rules",
        json!({
            "name": "narrowed",
            "format": "npm",
            "patterns": ["@acme/*"],
            "effect": "deny",
            "scope": ["all"],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A pattern that filters every name of its format is written on purpose.
    let all = post(
        &server,
        "/api/v1/routing-rules",
        json!({ "name": "everything", "format": "npm", "patterns": ["*"], "effect": "deny" }),
    )
    .await;
    assert_eq!(all.status(), StatusCode::BAD_REQUEST);

    // A target that is not a hosted repository is refused at write time,
    // never at resolution time.
    let bad_target = post(
        &server,
        "/api/v1/routing-rules",
        json!({
            "name": "proxy-target",
            "format": "npm",
            "patterns": ["@acme/*"],
            "effect": "allow_members",
            "targets": ["public"],
        }),
    )
    .await;
    assert_eq!(bad_target.status(), StatusCode::BAD_REQUEST);
}

/// T21 again, the half that matters: a repository created after the rule is
/// covered by it without the rule changing.
#[tokio::test]
async fn a_repository_created_later_is_covered_at_once() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;

    let created = post(
        &server,
        "/api/v1/repositories",
        json!({
            "name": "second-proxy",
            "type": "proxy",
            "format": "npm",
            "visibility": "public",
            "upstream": up.url(),
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let before = up.hits().len();
    assert_eq!(
        status_of(&server, &format!("/second-proxy/{PINNED}")).await,
        StatusCode::NOT_FOUND,
        "a protection that a new repository escapes is not a protection"
    );
    assert_eq!(up.hits().len(), before);
}

/// T26: deleting a rule reopens what it refused — and says so only when it
/// really does.
#[tokio::test]
async fn deleting_a_rule_says_what_it_reopens() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;
    create_rule(
        &server,
        json!({
            "name": "acme-deny",
            "format": "npm",
            "patterns": ["@acme/*"],
            "effect": "deny",
        }),
    )
    .await;

    let first: Value = reqwest::Client::new()
        .delete(format!("{}/api/v1/routing-rules/acme-internal", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("request failed")
        .json()
        .await
        .expect("invalid json");
    assert_eq!(first["still_refused_by"], json!(["acme-deny"]));
    assert_eq!(
        status_of(&server, &format!("/all/{PINNED}")).await,
        StatusCode::NOT_FOUND,
        "the other rule still refuses"
    );
    assert!(up.hits().is_empty());

    let second: Value = reqwest::Client::new()
        .delete(format!("{}/api/v1/routing-rules/acme-deny", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .expect("request failed")
        .json()
        .await
        .expect("invalid json");
    assert_eq!(second["still_refused_by"], json!([]));
    assert_eq!(status_of(&server, &format!("/all/{PINNED}")).await, StatusCode::OK);
    assert!(!up.hits().is_empty(), "the path is open again");
}

/// I6: a refusal names no rule on the wire. The 404 a covered name gets is
/// the 404 the group already answered, with no header and no body saying why.
#[tokio::test]
async fn a_refusal_names_no_rule_to_the_client() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;

    let refused = reqwest::get(format!("{}/all/{PINNED}", server.base_url))
        .await
        .expect("request failed");
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    let headers = format!("{:?}", refused.headers());
    let body = refused.text().await.expect("a body");
    for said in ["acme-internal", "routing", "rule"] {
        assert!(!body.contains(said), "the body says {said}: {body}");
        assert!(!headers.contains(said), "a header says {said}: {headers}");
    }

    let absent = reqwest::get(format!("{}/all/@other/nothing", server.base_url))
        .await
        .expect("request failed");
    assert_eq!(
        absent.status(),
        StatusCode::NOT_FOUND,
        "a name no rule covers and no member has answers the same"
    );
}

/// The whole surface is admin-only: a rule is a security control, and reading
/// the rules is reading the shape of what is internal.
#[tokio::test]
async fn the_rules_are_admin_only() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    for path in ["/api/v1/routing-rules", "/api/v1/routing-rules/whatever"] {
        assert_eq!(
            status_of(&server, path).await,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
}

/// The audit entries whose action starts with `routing.refused`, on a
/// deadline: the record is written off the read path, so the client's answer
/// does not wait for it and neither does the assertion's first look.
async fn refusal_entries(server: &TestServer, expected: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let page: Value = reqwest::Client::new()
            .get(format!("{}/api/v1/system/audit?size=200", server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .expect("request failed")
            .json()
            .await
            .expect("invalid json");
        let found: Vec<Value> = page["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                e["action"]
                    .as_str()
                    .is_some_and(|a| a.starts_with("routing.refused"))
            })
            .collect();
        if found.len() >= expected || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::task::yield_now().await;
    }
}

/// T9: a hundred refusals leave one audit line **per triplet** — the name,
/// the repository addressed and the member left out — naming every rule that
/// refused and the caller that asked, and nothing after that.
#[tokio::test]
async fn a_refusal_leaves_one_audit_line_per_triplet() {
    let up = seed_upstream().await;
    let server = spawn_group(&up).await;
    create_rule(&server, pinned_to_internal()).await;
    create_rule(
        &server,
        json!({
            "name": "acme-deny",
            "format": "npm",
            "patterns": ["@acme/*"],
            "effect": "deny",
        }),
    )
    .await;

    for _ in 0..20 {
        assert_eq!(
            status_of(&server, &format!("/all/{PINNED}")).await,
            StatusCode::NOT_FOUND
        );
    }
    // Both members of the group are refused for this name, so two triplets
    // and two lines — twenty requests, not forty.
    let entries = refusal_entries(&server, 2).await;
    assert_eq!(entries.len(), 2, "one per triplet, then counters: {entries:?}");

    let mut by_member: Vec<(String, Value)> = entries
        .iter()
        .map(|e| {
            let details: Value =
                serde_json::from_str(e["details_json"].as_str().expect("details json"))
                    .expect("invalid details json");
            assert_eq!(e["target"], "all", "the repository the client addressed");
            assert_eq!(details["actor_kind"], "anonymous");
            assert_eq!(details["ident_key"], PINNED);
            (details["member"].as_str().unwrap().to_string(), details)
        })
        .collect();
    by_member.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        by_member.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
        ["internal", "public"]
    );
    assert_eq!(by_member[0].1["rules"], json!(["acme-deny"]));
    assert_eq!(
        by_member[1].1["rules"],
        json!(["acme-deny", "acme-internal"]),
        "every rule that refuses, in name order"
    );
}
