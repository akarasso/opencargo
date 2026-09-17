mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{
    build_cargo_publish_body, build_crate_data, build_npm_publish_body, build_tarball, hosted,
    spawn_server, SpawnOpts,
};
use opencargo::config::{RepositoryFormat, Visibility};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Publish an npm package to the test-npm repository.
async fn publish_npm_package(
    client: &reqwest::Client,
    base_url: &str,
    name: &str,
    version: &str,
    description: &str,
) {
    let pkg_json = format!(
        r#"{{"name":"{}","version":"{}","description":"{}","main":"index.js"}}"#,
        name, version, description
    );
    let tarball = build_tarball(&pkg_json);
    let body = build_npm_publish_body(name, version, description, &tarball);

    let resp = client
        .put(format!("{}/test-npm/{}", base_url, name))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "npm publish failed: {:?}",
        resp.text().await
    );
}

/// Start a test server on a random port with both npm and cargo repositories.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("test-npm", RepositoryFormat::Npm, Visibility::Public),
            hosted("cargo-private", RepositoryFormat::Cargo, Visibility::Private),
        ],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
}

// ===========================================================================
// Phase 4 -- UI Tests (SPA + JSON API)
// ===========================================================================

#[tokio::test]
async fn test_dashboard_page() {
    let (base_url, _handle, _tmp) = setup().await;

    // The SPA shell should be served at /
    let resp = reqwest::get(format!("{}/", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("opencargo"),
        "dashboard should contain 'opencargo'"
    );
    // SPA serves index.html with a script tag that loads the app
    assert!(
        body.contains("<div id=\"app\">"),
        "SPA shell should contain app mount point"
    );
}

#[tokio::test]
async fn test_dashboard_api() {
    let (base_url, _handle, _tmp) = setup().await;

    let resp = reqwest::get(format!("{}/api/v1/dashboard", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    assert!(
        data.get("total_packages").is_some(),
        "dashboard API should return total_packages"
    );
    assert!(
        data.get("total_repos").is_some(),
        "dashboard API should return total_repos"
    );
    assert!(
        data.get("recent_versions").is_some(),
        "dashboard API should return recent_versions"
    );
}

#[tokio::test]
async fn test_packages_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish a package first
    publish_npm_package(
        &client,
        &base_url,
        "@test/pkglist",
        "1.0.0",
        "A package for listing test",
    )
    .await;

    // SPA shell is served at /packages
    let resp = client
        .get(format!("{}/packages", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/packages", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    let packages = data["packages"].as_array().expect("packages should be an array");
    assert!(
        packages.iter().any(|p| p["name"].as_str() == Some("@test/pkglist")),
        "packages API should contain the published package name, data: {:?}",
        data
    );
}

#[tokio::test]
async fn test_package_detail_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish @test/uipkg
    publish_npm_package(
        &client,
        &base_url,
        "@test/uipkg",
        "2.0.0",
        "UI package test",
    )
    .await;

    // SPA shell is served at /packages/*
    let resp = client
        .get(format!("{}/packages/@test/uipkg", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/packages/@test/uipkg", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    assert_eq!(
        data["name"].as_str(),
        Some("@test/uipkg"),
        "package detail API should return '@test/uipkg'"
    );
    let versions = data["versions"].as_array().expect("versions should be an array");
    assert!(
        versions.iter().any(|v| v["version"].as_str() == Some("2.0.0")),
        "package detail should contain version '2.0.0'"
    );
}

/// A9 + S17: a README sent at publish time is persisted (the `packages.readme`
/// column used to stay empty) and surfaced by the dashboard API as rendered
/// HTML, with embedded markup sanitized — no live <script> survives ammonia.
#[tokio::test]
async fn test_npm_readme_persisted_and_sanitized() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let name = "@test/readmepkg";
    let tarball = build_tarball(
        r#"{"name":"@test/readmepkg","version":"1.0.0","description":"readme test","main":"index.js"}"#,
    );
    let mut body = build_npm_publish_body(name, "1.0.0", "readme test", &tarball);
    body["readme"] =
        json!("# Title Heading\n\nSome **bold** text.\n\n<script>alert('xss')</script>\n");

    let resp = client
        .put(format!("{}/test-npm/{}", base_url, name))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    let resp = client
        .get(format!("{}/api/v1/packages/{}", base_url, name))
        .send()
        .await
        .expect("package detail API request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    let readme_html = data["readme_html"].as_str().unwrap_or("");

    // A9: the README content is now present (the column was never populated).
    assert!(
        readme_html.contains("Title Heading"),
        "rendered README should contain the heading text, got: {readme_html}"
    );
    // S17: the embedded <script> must be stripped by ammonia.
    assert!(
        !readme_html.contains("<script"),
        "rendered README must not contain a live <script> tag, got: {readme_html}"
    );
}

/// T5 hardening: a README larger than the 256 KiB cap whose truncation point
/// lands inside a multi-byte UTF-8 character must not panic the publish handler
/// (a naive `&s[..cap]` slice would). The cap recedes to a char boundary, so the
/// publish succeeds and the truncated README still renders.
#[tokio::test]
async fn test_npm_oversized_readme_truncates_without_panic() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let name = "@test/bigreadme";
    let tarball = build_tarball(
        r#"{"name":"@test/bigreadme","version":"1.0.0","description":"big readme","main":"index.js"}"#,
    );
    // 256 KiB - 1 ASCII bytes, then multi-byte chars so the cap lands mid-emoji,
    // exercising the char-boundary rewind.
    let mut readme = "a".repeat(256 * 1024 - 1);
    readme.push_str(&"😀".repeat(8));
    let mut body = build_npm_publish_body(name, "1.0.0", "big readme", &tarball);
    body["readme"] = json!(readme);

    let resp = client
        .put(format!("{}/test-npm/{}", base_url, name))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "oversized multi-byte README must not panic the publish"
    );

    // The truncated README is still served (rendered).
    let resp = client
        .get(format!("{}/api/v1/packages/{}", base_url, name))
        .send()
        .await
        .expect("package detail API request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let data: Value = resp.json().await.expect("invalid JSON");
    assert!(
        data["readme_html"].as_str().unwrap_or("").contains("aaaa"),
        "truncated README should still render its ASCII content"
    );
}

#[tokio::test]
async fn test_search_page() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish @test/searchable
    publish_npm_package(
        &client,
        &base_url,
        "@test/searchable",
        "1.0.0",
        "A searchable package",
    )
    .await;

    // SPA shell is served at /search
    let resp = client
        .get(format!("{}/search?q=searchable", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Test the JSON API that the SPA calls
    let resp = client
        .get(format!("{}/api/v1/search?q=searchable", base_url))
        .send()
        .await
        .expect("API request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let data: Value = resp.json().await.expect("invalid JSON");
    let results = data["results"].as_array().expect("results should be an array");
    assert!(
        results.iter().any(|r| r["name"].as_str() == Some("@test/searchable")),
        "search API should contain '@test/searchable'"
    );
}

#[tokio::test]
async fn test_static_css() {
    let (base_url, _handle, _tmp) = setup().await;

    // First, get the SPA shell to find the CSS asset path
    let resp = reqwest::get(format!("{}/", base_url))
        .await
        .expect("request failed");

    let body = resp.text().await.expect("failed to read body");

    // Extract the CSS asset path from the HTML (e.g., /assets/index-XXXX.css)
    let css_path = body
        .split("href=\"")
        .find(|s| s.contains(".css"))
        .and_then(|s| s.split('"').next())
        .expect("should find a CSS asset link in the SPA shell");

    let resp = reqwest::get(format!("{}{}", base_url, css_path))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type header")
        .to_str()
        .expect("invalid content-type");

    assert!(
        content_type.contains("css"),
        "content-type should contain 'css', got: {}",
        content_type
    );
}

// ===========================================================================
// Phase 5 -- Metrics Tests
// ===========================================================================

#[tokio::test]
async fn test_prometheus_metrics() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Make some requests to generate metrics
    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("health request failed");

    // GET /metrics
    let resp = client
        .get(format!("{}/metrics", base_url))
        .send()
        .await
        .expect("metrics request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read metrics body");
    assert!(
        body.contains("opencargo_http_requests_total"),
        "metrics should contain 'opencargo_http_requests_total', body: {}",
        body
    );
    assert!(
        body.contains("opencargo_http_request_duration_seconds"),
        "metrics should contain 'opencargo_http_request_duration_seconds', body: {}",
        body
    );
}

#[tokio::test]
async fn test_metrics_counts_requests() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Make at least 2 requests to /health/live
    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("first health request failed");

    let _ = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("second health request failed");

    // GET /metrics
    let resp = client
        .get(format!("{}/metrics", base_url))
        .send()
        .await
        .expect("metrics request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.expect("failed to read metrics body");

    // Find the counter line for path="/health/live"
    // The Prometheus text format looks like:
    //   opencargo_http_requests_total{method="GET",path="/health/live",status="200"} 2
    let found = body.lines().any(|line| {
        line.contains("opencargo_http_requests_total")
            && line.contains("path=\"/health/live\"")
            && !line.starts_with('#')
            && {
                // Parse the numeric value at the end of the line
                if let Some(val_str) = line.split_whitespace().last() {
                    if let Ok(val) = val_str.parse::<f64>() {
                        val >= 2.0
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
    });

    assert!(
        found,
        "metrics should show at least 2 requests to /health/live, metrics:\n{}",
        body
    );
}

/// Regression for the metrics-cardinality DoS: the Prometheus `path` label must
/// be the matched route TEMPLATE, not the raw URI. Two requests to the same
/// route with different dynamic segments must NOT leak those segments as label
/// values (attacker-chosen paths would otherwise grow the series set without
/// bound — memory exhaustion).
#[tokio::test]
async fn test_metrics_path_label_is_bounded() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    for name in ["zzcardunique1", "zzcardunique2"] {
        let _ = client
            .get(format!("{}/test-npm/@test/{}", base_url, name))
            .send()
            .await
            .expect("request failed");
    }

    let resp = client
        .get(format!("{}/metrics", base_url))
        .send()
        .await
        .expect("metrics request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("failed to read metrics body");

    assert!(
        !body.contains("zzcardunique1") && !body.contains("zzcardunique2"),
        "raw dynamic path segments must not become Prometheus labels (cardinality DoS), metrics:\n{}",
        body
    );
}

/// Regression for the pagination overflow: a huge `page` must not overflow the
/// i64 OFFSET computation `(page-1)*PAGE_SIZE` (which panics in debug and wraps
/// to a negative OFFSET in release). The request must complete normally.
#[tokio::test]
async fn test_dashboard_pagination_huge_page_no_overflow() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/v1/packages?page=999999999999999999", base_url))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a huge page must not overflow into a 500"
    );
}

// ===========================================================================
// Phase 6 -- Cargo Registry Tests
// ===========================================================================

/// Regression test for S2: the dashboard API must not expose packages of a
/// PRIVATE repository to anonymous/non-admin callers, while admins still see
/// them. cargo-private (in setup()) is a private repo.
#[tokio::test]
async fn test_dashboard_hides_private_packages() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish a crate into the PRIVATE repo as admin.
    let crate_data = build_crate_data();
    let metadata_json = r#"{"name":"test-crate","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"Secret","license":"MIT"}"#;
    let body = build_cargo_publish_body(metadata_json, &crate_data);
    let resp = client
        .put(format!("{}/cargo-private/api/v1/crates/new", base_url))
        .bearer_auth("test-token")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("cargo publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Anonymous dashboard detail must NOT reveal the private package (404).
    let resp = client
        .get(format!("{}/api/v1/packages/test-crate", base_url))
        .send()
        .await
        .expect("anonymous detail request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "anonymous caller must not see a private package via the dashboard"
    );

    // Admin sees it.
    let resp = client
        .get(format!("{}/api/v1/packages/test-crate", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("admin detail request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "admin must see the private package"
    );

    // Anonymous package list must not contain it either.
    let resp = client
        .get(format!("{}/api/v1/packages", base_url))
        .send()
        .await
        .expect("anonymous list request failed");
    let data: Value = resp.json().await.expect("invalid JSON");
    let empty = vec![];
    let names: Vec<&str> = data["packages"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"test-crate"),
        "anonymous package list must not include the private 'test-crate', got: {:?}",
        names
    );
}
