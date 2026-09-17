mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{build_npm_publish_body, build_tarball, hosted, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};

/// Start a test server on a random port with one public npm repository.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted("test-npm", RepositoryFormat::Npm, Visibility::Public)],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_check() {
    let (base_url, _handle, _tmp) = setup().await;

    let resp = reqwest::get(format!("{}/health/live", base_url))
        .await
        .expect("request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body, json!({"status": "ok"}));
}

#[tokio::test]
async fn test_publish_and_get_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(resp.status(), StatusCode::OK, "publish failed: {:?}", resp.text().await);

    // Fetch metadata
    let resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .send()
        .await
        .expect("get metadata request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let meta: Value = resp.json().await.expect("invalid json");

    // Verify name
    assert_eq!(meta["name"], "@test/hello");

    // Verify dist-tags
    assert_eq!(meta["dist-tags"]["latest"], "1.0.0");

    // Verify version entry exists
    assert!(meta["versions"]["1.0.0"].is_object(), "version 1.0.0 not found in metadata");
    assert_eq!(meta["versions"]["1.0.0"]["version"], "1.0.0");

    // Verify tarball URL format
    let tarball_url = meta["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .expect("no tarball url");
    assert!(
        tarball_url.contains("/test-npm/@test/hello/-/hello-1.0.0.tgz"),
        "unexpected tarball URL: {}",
        tarball_url
    );
    assert!(
        tarball_url.starts_with(&base_url),
        "tarball URL should start with base_url"
    );
}

#[tokio::test]
async fn test_unscoped_publish_and_get_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"plainpkg","version":"2.0.0","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("plainpkg", "2.0.0", "", &tarball);

    let resp = client
        .put(format!("{}/test-npm/plainpkg", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish failed: {:?}", resp.text().await);

    let resp = client
        .get(format!("{}/test-npm/plainpkg", base_url))
        .send()
        .await
        .expect("get metadata request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "unscoped metadata must not fall through to the SPA"
    );
    let meta: Value = resp.json().await.expect("invalid json");
    assert_eq!(meta["name"], "plainpkg");
    assert_eq!(meta["dist-tags"]["latest"], "2.0.0");
    let tarball_url = meta["versions"]["2.0.0"]["dist"]["tarball"].as_str().unwrap();
    assert!(tarball_url.contains("/test-npm/plainpkg/-/plainpkg-2.0.0.tgz"), "{tarball_url}");

    let resp = client.get(tarball_url).send().await.expect("tarball request failed");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_download_tarball() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Download the tarball
    let resp = client
        .get(format!(
            "{}/test-npm/@test/hello/-/hello-1.0.0.tgz",
            base_url
        ))
        .send()
        .await
        .expect("download request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let downloaded = resp.bytes().await.expect("failed to read tarball bytes");
    assert_eq!(
        downloaded.as_ref(),
        tarball.as_slice(),
        "downloaded tarball does not match the original"
    );
}

#[tokio::test]
async fn test_publish_duplicate_version() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // First publish — should succeed
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("first publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Second publish of the same version — should get 409 Conflict
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("second publish failed");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_search() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Search
    let resp = client
        .get(format!("{}/test-npm/-/v1/search?text=hello", base_url))
        .send()
        .await
        .expect("search request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let search_result: Value = resp.json().await.expect("invalid json");
    let objects = search_result["objects"]
        .as_array()
        .expect("objects should be an array");

    assert!(
        !objects.is_empty(),
        "search should return at least one result"
    );

    let found = objects
        .iter()
        .any(|o| o["package"]["name"].as_str() == Some("@test/hello"));
    assert!(found, "search results should contain @test/hello");

    assert_eq!(
        search_result["total"]
            .as_u64()
            .expect("total should be a number"),
        1
    );
}

/// Regression for the unbounded `size` search bug: a negative `size` must be
/// clamped, not passed straight through as `LIMIT -1` (unlimited in SQLite),
/// which would bypass the 250 cap and amplify the per-result N+1 lookups. A
/// normal search finds all matches; size=-1 must not return the whole repo.
#[tokio::test]
async fn test_npm_search_size_is_clamped() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    for i in 1..=3 {
        let name = format!("@test/clamppkg{i}");
        let pkg_json = format!(
            r#"{{"name":"{name}","version":"1.0.0","description":"zzclampword pkg","main":"index.js"}}"#
        );
        let tarball = build_tarball(&pkg_json);
        let body = build_npm_publish_body(&name, "1.0.0", "zzclampword pkg", &tarball);
        let resp = client
            .put(format!("{}/test-npm/{}", base_url, name))
            .bearer_auth("test-token")
            .json(&body)
            .send()
            .await
            .expect("publish request failed");
        assert_eq!(resp.status(), StatusCode::OK, "publish {name} should succeed");
    }

    // A normal search finds all three.
    let resp = client
        .get(format!("{}/test-npm/-/v1/search?text=zzclampword&size=250", base_url))
        .send()
        .await
        .expect("search request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let data: Value = resp.json().await.expect("invalid json");
    let n_full = data["objects"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(n_full, 3, "a normal search should find all three packages");

    // A negative size must be clamped (no LIMIT -1 leak of the whole repo).
    let resp = client
        .get(format!("{}/test-npm/-/v1/search?text=zzclampword&size=-1", base_url))
        .send()
        .await
        .expect("search request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let data: Value = resp.json().await.expect("invalid json");
    let n_neg = data["objects"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        n_neg < 3,
        "negative size must be clamped, got {n_neg} objects (LIMIT -1 leak?)"
    );
}

#[tokio::test]
async fn test_abbreviated_metadata() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Publish
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Full metadata request (no special Accept header)
    let full_resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .send()
        .await
        .expect("full metadata request failed");
    assert_eq!(full_resp.status(), StatusCode::OK);
    let full_meta: Value = full_resp.json().await.expect("invalid json");

    // Abbreviated metadata request
    let abbrev_resp = client
        .get(format!("{}/test-npm/@test/hello", base_url))
        .header("Accept", "application/vnd.npm.install-v1+json")
        .send()
        .await
        .expect("abbreviated metadata request failed");
    assert_eq!(abbrev_resp.status(), StatusCode::OK);

    // Verify content-type header
    let content_type = abbrev_resp
        .headers()
        .get("content-type")
        .expect("missing content-type header")
        .to_str()
        .expect("invalid content-type");
    assert!(
        content_type.contains("application/vnd.npm.install-v1+json"),
        "unexpected content-type: {}",
        content_type
    );

    let abbrev_meta: Value = abbrev_resp.json().await.expect("invalid json");

    // The abbreviated version should still have name, version, dist
    let abbrev_version = &abbrev_meta["versions"]["1.0.0"];
    assert!(abbrev_version.is_object(), "abbreviated version 1.0.0 missing");
    assert!(abbrev_version.get("name").is_some(), "abbreviated should have name");
    assert!(abbrev_version.get("version").is_some(), "abbreviated should have version");
    assert!(abbrev_version.get("dist").is_some(), "abbreviated should have dist");

    // The abbreviated version should NOT have "main" or "description"
    // (these are stripped by the server for install-optimized responses)
    assert!(
        abbrev_version.get("main").is_none(),
        "abbreviated should NOT have 'main' field"
    );
    assert!(
        abbrev_version.get("description").is_none(),
        "abbreviated should NOT have 'description' field"
    );

    // The full version should have those fields
    let full_version = &full_meta["versions"]["1.0.0"];
    assert!(
        full_version.get("main").is_some(),
        "full metadata should have 'main' field"
    );
    assert!(
        full_version.get("description").is_some(),
        "full metadata should have 'description' field"
    );
}

#[tokio::test]
async fn test_auth_required_for_publish() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/hello","version":"1.0.0","description":"Test package","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@test/hello", "1.0.0", "Test package", &tarball);

    // Attempt to publish WITHOUT a Bearer token
    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .json(&body)
        .send()
        .await
        .expect("publish request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "publish without token should return 401"
    );
}

/// `npm deprecate`: a PUT carrying an existing version flagged `deprecated`
/// (and no _attachments) updates that version's metadata instead of 409-ing,
/// and the flag is then served in the package document.
#[tokio::test]
async fn test_npm_deprecate() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Publish version 1.0.0 first.
    let tarball = build_tarball(r#"{"name":"@test/deprecateme","version":"1.0.0"}"#);
    let body = build_npm_publish_body("@test/deprecateme", "1.0.0", "to deprecate", &tarball);
    let resp = client
        .put(format!("{}/test-npm/@test/deprecateme", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");

    // Deprecate it: same route, version flagged `deprecated`, NO _attachments.
    let deprecate_body = json!({
        "name": "@test/deprecateme",
        "versions": {
            "1.0.0": {
                "name": "@test/deprecateme",
                "version": "1.0.0",
                "deprecated": "use @test/newpkg instead"
            }
        }
    });
    let resp = client
        .put(format!("{}/test-npm/@test/deprecateme", base_url))
        .bearer_auth("test-token")
        .json(&deprecate_body)
        .send()
        .await
        .expect("deprecate request failed");
    assert!(
        resp.status().is_success(),
        "deprecate should succeed, got {}",
        resp.status()
    );

    // The package document now exposes `deprecated` on the version.
    let resp = client
        .get(format!("{}/test-npm/@test/deprecateme", base_url))
        .send()
        .await
        .expect("get metadata request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let doc: Value = resp.json().await.expect("invalid json");
    assert_eq!(
        doc["versions"]["1.0.0"]["deprecated"].as_str(),
        Some("use @test/newpkg instead"),
        "version 1.0.0 should be flagged deprecated, got: {:?}",
        doc["versions"]["1.0.0"]
    );

    // Undeprecate (empty message) removes the flag.
    let undeprecate_body = json!({
        "name": "@test/deprecateme",
        "versions": {
            "1.0.0": { "name": "@test/deprecateme", "version": "1.0.0", "deprecated": "" }
        }
    });
    let resp = client
        .put(format!("{}/test-npm/@test/deprecateme", base_url))
        .bearer_auth("test-token")
        .json(&undeprecate_body)
        .send()
        .await
        .expect("undeprecate request failed");
    assert!(resp.status().is_success(), "undeprecate should succeed");

    let resp = client
        .get(format!("{}/test-npm/@test/deprecateme", base_url))
        .send()
        .await
        .expect("get metadata request failed");
    let doc: Value = resp.json().await.expect("invalid json");
    assert!(
        doc["versions"]["1.0.0"].get("deprecated").is_none(),
        "deprecated flag should be removed after undeprecate"
    );
}

/// Hostile or malformed package names must be rejected with 400 at publish,
/// before anything reaches the DB or the storage tree.
#[tokio::test]
async fn test_npm_publish_rejects_invalid_name() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Uppercase scope: npm names are lowercase-only.
    let pkg_json = r#"{"name":"@Test/hello","version":"1.0.0","description":"x","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    let body = build_npm_publish_body("@Test/hello", "1.0.0", "x", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@Test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an uppercase npm name must be rejected with 400"
    );

    // Leading dot in the name part.
    let body = build_npm_publish_body("@test/.hidden", "1.0.0", "x", &tarball);
    let resp = client
        .put(format!("{}/test-npm/@test/.hidden", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a leading-dot npm name must be rejected with 400"
    );
}

/// The `name` in the publish body must match the package name in the URL —
/// otherwise the tarball is stored under one name while the metadata claims
/// another.
#[tokio::test]
async fn test_npm_publish_rejects_body_name_mismatch() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let pkg_json = r#"{"name":"@test/other","version":"1.0.0","description":"x","main":"index.js"}"#;
    let tarball = build_tarball(pkg_json);
    // Body says "@test/other" but the URL targets "@test/hello".
    let body = build_npm_publish_body("@test/other", "1.0.0", "x", &tarball);

    let resp = client
        .put(format!("{}/test-npm/@test/hello", base_url))
        .bearer_auth("test-token")
        .json(&body)
        .send()
        .await
        .expect("publish request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a body/URL package-name mismatch must be rejected with 400"
    );
}

/// `npm dist-tag ls|add|rm <pkg>` on an unscoped package addresses
/// `/{repo}/-/package/{name}/dist-tags[/{tag}]`; only the scoped pair used to
/// be routed, so every one of these 404'd on hosted repos.
#[tokio::test]
async fn unscoped_dist_tags_get_put_delete() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    for version in ["1.0.0", "1.1.0"] {
        let pkg_json = format!(r#"{{"name":"tagpkg","version":"{version}","main":"index.js"}}"#);
        let tarball = build_tarball(&pkg_json);
        let body = build_npm_publish_body("tagpkg", version, "Tagged package", &tarball);
        let resp = client
            .put(format!("{}/test-npm/tagpkg", base_url))
            .bearer_auth("test-token")
            .json(&body)
            .send()
            .await
            .expect("publish request failed");
        assert_eq!(resp.status(), StatusCode::OK, "publish {version} failed");
    }

    let dist_tags_url = format!("{}/test-npm/-/package/tagpkg/dist-tags", base_url);
    let resp = client.get(&dist_tags_url).send().await.expect("get dist-tags failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let tags: Value = resp.json().await.expect("invalid json");
    assert_eq!(tags, json!({"latest": "1.1.0"}));

    let resp = client
        .put(format!("{dist_tags_url}/beta"))
        .bearer_auth("test-token")
        .json(&json!("1.0.0"))
        .send()
        .await
        .expect("put dist-tag failed");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);

    let tags: Value = client.get(&dist_tags_url).send().await.unwrap().json().await.unwrap();
    assert_eq!(tags, json!({"latest": "1.1.0", "beta": "1.0.0"}));

    let resp = client
        .delete(format!("{dist_tags_url}/beta"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete dist-tag failed");
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);

    let tags: Value = client.get(&dist_tags_url).send().await.unwrap().json().await.unwrap();
    assert_eq!(tags, json!({"latest": "1.1.0"}));

    let resp = client
        .put(format!("{dist_tags_url}/beta"))
        .json(&json!("1.0.0"))
        .send()
        .await
        .expect("anonymous put dist-tag failed");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "dist-tag writes need a token");
}
