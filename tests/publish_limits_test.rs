mod common;

use reqwest::{Response, StatusCode};
use serde_json::{json, Value};

use common::{
    build_cargo_publish_body, build_npm_publish_body, build_tarball, hosted, limits, pypi,
    spawn_server, SpawnOpts, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const REPOS: [(&str, RepositoryFormat); 4] = [
    ("npm-private", RepositoryFormat::Npm),
    ("npm-ci", RepositoryFormat::Npm),
    ("pypi-private", RepositoryFormat::Pypi),
    ("crates", RepositoryFormat::Cargo),
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
