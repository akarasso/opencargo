mod common;

use reqwest::{Response, StatusCode};
use serde_json::{json, Value};

use common::{
    build_cargo_publish_body, build_npm_publish_body, build_tarball, hosted, limits, push_blob,
    pypi, spawn_server, SpawnOpts, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const REPOS: [(&str, RepositoryFormat); 7] = [
    ("npm-private", RepositoryFormat::Npm),
    ("npm-ci", RepositoryFormat::Npm),
    ("pypi-private", RepositoryFormat::Pypi),
    ("crates", RepositoryFormat::Cargo),
    ("files", RepositoryFormat::Raw),
    ("servers", RepositoryFormat::Mcp),
    ("images", RepositoryFormat::Oci),
];

async fn server(section: &str) -> common::TestServer {
    spawn_server(SpawnOpts {
        repositories: REPOS
            .iter()
            .map(|(name, fmt)| hosted(name, *fmt, Visibility::Public))
            .collect(),
        limits: limits(section),
        ..Default::default()
    })
    .await
}

async fn publish_npm(
    client: &reqwest::Client,
    base_url: &str,
    repo: &str,
    token: &str,
    n: usize,
) -> Response {
    let name = format!("pkg-{n}");
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"1.0.0"}}"#));
    let body = build_npm_publish_body(&name, "1.0.0", "batch", &tarball);
    client
        .put(format!("{base_url}/{repo}/{name}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("publish request failed")
}

async fn publish_cargo(client: &reqwest::Client, base_url: &str, n: usize) -> Response {
    let meta = json!({"name": format!("krate-{n}"), "vers": "1.0.0"}).to_string();
    let body = build_cargo_publish_body(&meta, b"crate bytes");
    client
        .put(format!("{base_url}/crates/api/v1/crates/new"))
        .bearer_auth(STATIC_TOKEN)
        .body(body)
        .send()
        .await
        .expect("publish request failed")
}

fn refusal(response: &Response) -> u64 {
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("a refusal carries Retry-After")
        .to_str()
        .expect("an ASCII Retry-After")
        .parse()
        .expect("Retry-After is a number of seconds")
}

#[tokio::test]
async fn the_shipped_default_is_thirty_npm_publishes_a_minute() {
    let server = server("").await;
    let client = reqwest::Client::new();
    for n in 0..30 {
        let resp = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "publish {n} was refused");
    }
    let refused = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, 30).await;
    let retry_after = refusal(&refused);
    assert!(
        (1..=61).contains(&retry_after),
        "Retry-After {retry_after} is outside the window"
    );
    let body: Value = refused.json().await.expect("a json refusal");
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("30 per 60s"),
        "the refusal names the limit it hit: {message}"
    );
}

#[tokio::test]
async fn an_overridden_format_limit_is_the_one_enforced() {
    let server = server("[limits.publish.format]\nnpm = 3\n").await;
    let client = reqwest::Client::new();
    for n in 0..3 {
        let resp = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "publish {n} was refused");
    }
    let refused = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, 3).await;
    refusal(&refused);
    let body: Value = refused.json().await.expect("a json refusal");
    assert!(
        body["error"].as_str().is_some_and(|m| m.contains("3 per 60s")),
        "the refusal names the configured limit: {body:?}"
    );
}

#[tokio::test]
async fn a_ci_repository_publishes_a_batch_while_its_format_stays_capped() {
    let server = server(
        "[limits.publish.format]\nnpm = 2\n[limits.publish.repository]\nnpm-ci = { max = 40, per = \"1h\" }\n",
    )
    .await;
    let client = reqwest::Client::new();
    for n in 0..40 {
        let resp = publish_npm(&client, &server.base_url, "npm-ci", STATIC_TOKEN, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "batch publish {n} was refused");
    }
    refusal(&publish_npm(&client, &server.base_url, "npm-ci", STATIC_TOKEN, 40).await);

    for n in 100..102 {
        let resp = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "publish {n} was refused");
    }
    refusal(&publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, 102).await);
}

#[tokio::test]
async fn the_fallback_meters_a_format_that_ships_unmetered() {
    let unmetered = server("").await;
    let client = reqwest::Client::new();
    for n in 0..4 {
        let resp = publish_cargo(&client, &unmetered.base_url, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "publish {n} was refused");
    }

    let metered = server("[limits.publish]\nper_window = 2\n").await;
    for n in 0..2 {
        let resp = publish_cargo(&client, &metered.base_url, n).await;
        assert_eq!(resp.status(), StatusCode::OK, "publish {n} was refused");
    }
    refusal(&publish_cargo(&client, &metered.base_url, 2).await);
}

#[tokio::test]
async fn a_pypi_upload_is_metered_by_its_own_entry() {
    let server = server("[limits.publish.format]\npypi = 1\n").await;
    let client = reqwest::Client::new();
    let authorization = pypi::basic("__token__", STATIC_TOKEN);
    let first = pypi::upload(
        &client,
        &server.base_url,
        "pypi-private",
        &authorization,
        &pypi::wheel_name("widget", "1.0.0"),
        &pypi::wheel("widget", "1.0.0", None),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK, "the first upload was refused");

    let refused = pypi::upload(
        &client,
        &server.base_url,
        "pypi-private",
        &authorization,
        &pypi::wheel_name("widget", "2.0.0"),
        &pypi::wheel("widget", "2.0.0", None),
    )
    .await;
    refusal(&refused);
}

#[tokio::test]
async fn a_raw_put_is_metered_by_its_own_entry() {
    let server = server("[limits.publish.format]\nraw = 1\n").await;
    let client = reqwest::Client::new();
    let put = |path: &str| {
        client
            .put(format!("{}/raw/files/{path}", server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .body("bytes")
            .send()
    };
    assert_eq!(put("dist/a.bin").await.unwrap().status(), StatusCode::CREATED);
    refusal(&put("dist/b.bin").await.unwrap());
}

/// The two ways into an mcp repository spend one allowance: the meter runs
/// before a body is read, so the skill route is refused on the count alone.
#[tokio::test]
async fn an_mcp_publish_and_a_skill_upload_share_the_mcp_meter() {
    let server = server("[limits.publish.format]\nmcp = 1\n").await;
    let client = reqwest::Client::new();
    let published = client
        .post(format!("{}/servers/v0.1/publish", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&common::mcp::record("io.github.acme/billing", "1.0.0"))
        .send()
        .await
        .unwrap();
    assert_eq!(published.status(), StatusCode::CREATED, "{:?}", published.text().await);
    let skill = client
        .put(format!("{}/servers/skills/deploy/1.0.0/skill.zip", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(vec![0u8; 16])
        .send()
        .await
        .unwrap();
    refusal(&skill);
}

/// A push is a session of blob uploads closed by one manifest put, the
/// request that makes the image exist: that one is counted, the blobs are
/// not, so a count is an image count.
#[tokio::test]
async fn an_oci_push_is_metered_at_its_manifest_put() {
    let server = server("[limits.publish.format]\noci = 1\n").await;
    let client = reqwest::Client::new();
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer = b"layer bytes";
    let config_digest = push_blob(&client, &server.base_url, "images/app", config).await;
    let layer_digest = push_blob(&client, &server.base_url, "images/app", layer).await;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest, "size": layer.len()}],
    }))
    .unwrap();
    let put = |tag: &str| {
        client
            .put(format!("{}/v2/images/app/manifests/{tag}", server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .header("content-type", "application/vnd.oci.image.manifest.v1+json")
            .body(manifest.clone())
            .send()
    };
    assert_eq!(put("v1").await.unwrap().status(), StatusCode::CREATED);
    refusal(&put("v2").await.unwrap());
    push_blob(&client, &server.base_url, "images/app", b"another layer").await;
}

#[tokio::test]
async fn one_account_never_spends_another() {
    let server = server("[limits.publish.format]\nnpm = 1\n").await;
    let client = reqwest::Client::new();
    common::create_user(&client, &server.base_url, STATIC_TOKEN, "ci", "publisher").await;
    let created = client
        .post(format!("{}/api/v1/users/ci/tokens", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "name": "ci-token" }))
        .send()
        .await
        .expect("create token request failed");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created: Value = created.json().await.expect("a json token");
    let ci_token = created["token"].as_str().expect("a token").to_string();

    let resp = publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, 0).await;
    assert_eq!(resp.status(), StatusCode::OK);
    refusal(&publish_npm(&client, &server.base_url, "npm-private", STATIC_TOKEN, 1).await);

    let resp = publish_npm(&client, &server.base_url, "npm-private", &ci_token, 2).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "another account's window is its own"
    );
}
