//! The web UI's panels, which had no suite of their own while they were
//! twenty hand-written queries.
//!
//! Three things are pinned here. Every panel serves RFC 3339, not the stored
//! column — the audit log and a webhook registration served the raw column
//! until `DashboardRead` landed, so those two assertions go red on the
//! commit before this one. The search panel answers out of FTS5 with no
//! `LIKE` fallback underneath it, so a query that sanitises away is an empty
//! result rather than the 500 the deleted fallback was hiding. And a private
//! repository's package stays out of every panel an anonymous caller reads.

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::{build_npm_publish_body, build_tarball, hosted, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const PUBLIC: &str = "npm-public";
const PRIVATE: &str = "npm-private";
const OPEN_PKG: &str = "dashwidget";
const SECRET_PKG: &str = "secretwidget";

async fn server() -> common::TestServer {
    let srv = spawn_server(SpawnOpts {
        repositories: vec![
            hosted(PUBLIC, RepositoryFormat::Npm, Visibility::Public),
            hosted(PRIVATE, RepositoryFormat::Npm, Visibility::Private),
        ],
        ..Default::default()
    })
    .await;
    publish(&srv, PUBLIC, OPEN_PKG, "1.0.0").await;
    publish(&srv, PUBLIC, OPEN_PKG, "2.0.0").await;
    publish(&srv, PRIVATE, SECRET_PKG, "1.0.0").await;
    srv
}

async fn publish(srv: &common::TestServer, repo: &str, package: &str, version: &str) {
    let manifest = format!(r#"{{"name":"{package}","version":"{version}"}}"#);
    let body = build_npm_publish_body(package, version, "a widget", &build_tarball(&manifest));
    let resp = reqwest::Client::new()
        .put(format!("{}/{repo}/{package}", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("publish failed");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
}

/// `None` is an anonymous caller; `Some` the admin static token.
async fn get(srv: &common::TestServer, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = reqwest::Client::new().get(format!("{}{path}", srv.base_url));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let resp = request.send().await.expect("request failed");
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn rfc3339(label: &str, value: &Value) {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("{label} is not a string: {value}"));
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|err| panic!("{label} is not RFC 3339: {text:?} ({err})"));
}

fn names(results: &Value, key: &str) -> Vec<String> {
    results[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} is not an array: {results}"))
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect()
}

/// The stats panel: the counts an admin sees, and every stamp as RFC 3339.
#[tokio::test]
async fn the_stats_panel_counts_and_dates_what_was_published() {
    let srv = server().await;

    let (status, stats) = get(&srv, "/api/v1/dashboard", Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["total_packages"], json!(2));
    assert_eq!(stats["total_versions"], json!(3));
    assert_eq!(stats["total_repos"], json!(2));

    let recent = stats["recent_versions"]
        .as_array()
        .expect("recent_versions is not an array");
    assert_eq!(recent.len(), 3, "{recent:?}");
    for line in recent {
        rfc3339("recent_versions[].published_at", &line["published_at"]);
    }
}

/// The same panel for someone who is not an admin: the private repository's
/// package is not counted, and the repository count is the authenticated
/// one, which is a different rule and always has been.
#[tokio::test]
async fn the_stats_panel_counts_only_public_packages_for_everyone_else() {
    let srv = server().await;

    let (status, stats) = get(&srv, "/api/v1/dashboard", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["total_packages"], json!(1));
    assert_eq!(stats["total_versions"], json!(2));
    assert_eq!(
        stats["total_repos"],
        json!(1),
        "an anonymous caller counts public repositories only"
    );
}

/// The list panel: its page, its pager, what each package was last released
/// as, and its stamp.
#[tokio::test]
async fn the_list_panel_pages_packages_with_their_latest_version() {
    let srv = server().await;

    let (status, page) = get(&srv, "/api/v1/packages", Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], json!(2));
    assert_eq!(page["page"], json!(1));
    assert_eq!(page["has_next"], json!(false));

    let listed = page["packages"].as_array().expect("packages is not an array");
    let open = listed
        .iter()
        .find(|row| row["name"] == json!(OPEN_PKG))
        .unwrap_or_else(|| panic!("{OPEN_PKG} is not listed: {listed:?}"));
    assert_eq!(open["latest_version"], json!("2.0.0"));
    assert_eq!(open["description"], json!("a widget"));
    rfc3339("packages[].published_at", &open["published_at"]);

    let (_, filtered) = get(
        &srv,
        &format!("/api/v1/packages?repo={PUBLIC}&q=dash"),
        Some(STATIC_TOKEN),
    )
    .await;
    assert_eq!(names(&filtered, "packages"), vec![OPEN_PKG.to_string()]);

    let (_, anonymous) = get(&srv, "/api/v1/packages", None).await;
    assert_eq!(names(&anonymous, "packages"), vec![OPEN_PKG.to_string()]);
}

/// The detail panel, down to the version table's stamps.
#[tokio::test]
async fn the_detail_panel_lists_versions_newest_first() {
    let srv = server().await;

    let (status, detail) = get(
        &srv,
        &format!("/api/v1/packages/{OPEN_PKG}"),
        Some(STATIC_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["name"], json!(OPEN_PKG));

    let versions = detail["versions"].as_array().expect("versions is not an array");
    assert_eq!(
        versions
            .iter()
            .filter_map(|row| row["version"].as_str())
            .collect::<Vec<_>>(),
        vec!["2.0.0", "1.0.0"]
    );
    for version in versions {
        rfc3339("versions[].published_at", &version["published_at"]);
        assert!(
            version["size_display"].as_str().is_some_and(|s| !s.is_empty()),
            "a version serves its size: {version}"
        );
    }

    let (status, _) = get(&srv, &format!("/api/v1/packages/{SECRET_PKG}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a private package is a 404 for an anonymous caller"
    );
}

/// The search panel answers out of the index, with no `LIKE` query behind it
/// to absorb a failure.
#[tokio::test]
async fn the_search_panel_finds_a_published_package() {
    let srv = server().await;

    let (status, found) = get(&srv, "/api/v1/search?q=dashwidget", Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["query"], json!("dashwidget"));

    let results = found["results"].as_array().expect("results is not an array");
    let hit = results
        .iter()
        .find(|row| row["name"] == json!(OPEN_PKG))
        .unwrap_or_else(|| panic!("{OPEN_PKG} was not found: {results:?}"));
    assert_eq!(hit["latest_version"], json!("2.0.0"));
}

/// A query with nothing to match in it never reaches FTS5, which would
/// refuse the empty expression it would be handed. The deleted `LIKE`
/// fallback is what used to hide that; an empty result is the answer, and a
/// 500 is not.
#[tokio::test]
async fn a_query_that_sanitises_away_is_an_empty_result_not_a_failure() {
    let srv = server().await;

    for query in ["", "%20", "%20%20", "%22"] {
        let (status, found) = get(
            &srv,
            &format!("/api/v1/search?q={query}"),
            Some(STATIC_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "?q={query} answered {status}");
        assert_eq!(
            found["results"],
            json!([]),
            "?q={query} matches nothing, so it returns nothing"
        );
    }
}

/// The search scope is the visibility predicate the panel used to splice
/// into its SQL: a private package is the admin's to find and nobody else's.
#[tokio::test]
async fn the_search_panel_keeps_a_private_package_to_admins() {
    let srv = server().await;

    let (_, anonymous) = get(&srv, "/api/v1/search?q=secretwidget", None).await;
    assert_eq!(
        anonymous["results"],
        json!([]),
        "an anonymous caller must not find a private package"
    );

    let (_, admin) = get(&srv, "/api/v1/search?q=secretwidget", Some(STATIC_TOKEN)).await;
    assert_eq!(names(&admin, "results"), vec![SECRET_PKG.to_string()]);
}

/// The repository panel: an anonymous caller sees the public repositories
/// and no upstream URL, which can carry credentials in its userinfo.
#[tokio::test]
async fn the_repository_panel_hides_private_repositories_from_anonymous_callers() {
    let srv = server().await;

    let (status, listed) = get(&srv, "/api/v1/repositories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&listed, "repositories"), vec![PUBLIC.to_string()]);
    assert_eq!(listed["repositories"][0]["visibility"], json!("public"));

    let (_, all) = get(&srv, "/api/v1/repositories", Some(STATIC_TOKEN)).await;
    assert_eq!(
        names(&all, "repositories"),
        vec![PRIVATE.to_string(), PUBLIC.to_string()],
        "by name, which is the order the panel renders"
    );
}

/// A webhook registration and the audit entry its creation writes: both
/// served the stored column verbatim, in SQLite's `YYYY-MM-DD HH:MM:SS`.
#[tokio::test]
async fn a_registration_and_its_audit_entry_serve_rfc_3339() {
    let srv = spawn_server(SpawnOpts::default()).await;

    let created: Value = reqwest::Client::new()
        .post(format!("{}/api/v1/webhooks", srv.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({"url": "https://example.com/hook", "events": ["package.published"]}))
        .send()
        .await
        .expect("webhook creation failed")
        .json()
        .await
        .expect("webhook is not json");
    rfc3339("webhook created_at", &created["created_at"]);
    rfc3339("webhook updated_at", &created["updated_at"]);

    let (status, listed) = get(&srv, "/api/v1/webhooks", Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    rfc3339("webhooks[].created_at", &listed["webhooks"][0]["created_at"]);

    let (status, audit) = get(&srv, "/api/v1/system/audit", Some(STATIC_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    let entries = audit["entries"].as_array().expect("entries is not an array");
    let entry = entries
        .iter()
        .find(|row| row["action"] == json!("webhook.create"))
        .unwrap_or_else(|| panic!("no webhook.create entry: {entries:?}"));
    rfc3339("audit entry created_at", &entry["created_at"]);
}
