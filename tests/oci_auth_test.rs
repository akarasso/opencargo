mod common;

use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};

use common::{
    basic_auth_header, create_user, hosted, push_blob, sha256_digest, spawn_server, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const USER: &str = "docker-user";
const PASSWORD: &str = "docker-pass-123";

async fn spawn(anonymous_read: bool) -> TestServer {
    spawn_server(SpawnOpts {
        anonymous_read,
        repositories: vec![
            hosted("oci-public", RepositoryFormat::Oci, Visibility::Public),
            hosted("oci-private", RepositoryFormat::Oci, Visibility::Private),
        ],
        ..Default::default()
    })
    .await
}

fn realm(base_url: &str) -> String {
    format!("Bearer realm=\"{base_url}/v2/token\",service=\"opencargo\"")
}

fn header(resp: &Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name} header"))
        .to_str()
        .unwrap()
        .to_string()
}

async fn set_password(base_url: &str, username: &str, password: &str) {
    let resp = Client::new()
        .put(format!("{base_url}/api/v1/users/{username}/password"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "new_password": password }))
        .send()
        .await
        .expect("change password request failed");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn publisher(base_url: &str) {
    create_user(&Client::new(), base_url, STATIC_TOKEN, USER, "publisher").await;
    set_password(base_url, USER, PASSWORD).await;
}

/// Push a one-layer image tagged `1.0` into `image` (`{repo}/{name}`).
async fn seed_image(base_url: &str, image: &str) {
    let client = Client::new();
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer = b"layer-bytes";
    let config_digest = push_blob(&client, base_url, image, config).await;
    let layer_digest = push_blob(&client, base_url, image, layer).await;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer.len()
        }]
    });
    let resp = client
        .put(format!("{base_url}/v2/{image}/manifests/1.0"))
        .bearer_auth(STATIC_TOKEN)
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(serde_json::to_vec(&manifest).unwrap())
        .send()
        .await
        .expect("put manifest failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// `GET /v2/token` with the given `Authorization` header, if any.
async fn token_response(base_url: &str, authorization: Option<&str>, scope: &str) -> Response {
    let mut req = Client::new().get(format!("{base_url}/v2/token?service=opencargo{scope}"));
    if let Some(value) = authorization {
        req = req.header("Authorization", value);
    }
    req.send().await.expect("token request failed")
}

/// The token `GET /v2/token` issues for `authorization`, checking the body shape.
async fn token(base_url: &str, authorization: Option<&str>, scope: &str) -> String {
    let resp = token_response(base_url, authorization, scope).await;
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    let body: Value = resp.json().await.expect("invalid json");
    let token = body["token"].as_str().expect("token").to_string();
    assert_eq!(body["access_token"], body["token"]);
    assert_eq!(body["expires_in"], 3600);
    chrono::DateTime::parse_from_rfc3339(body["issued_at"].as_str().expect("issued_at"))
        .expect("issued_at is RFC 3339");
    assert!(token.starts_with("ocr_"), "{token}");
    token
}

async fn ping(base_url: &str, token: &str) -> Response {
    Client::new()
        .get(format!("{base_url}/v2/"))
        .bearer_auth(token)
        .send()
        .await
        .expect("ping failed")
}

async fn start_upload(base_url: &str, image: &str, token: &str) -> Response {
    Client::new()
        .post(format!("{base_url}/v2/{image}/blobs/uploads/"))
        .bearer_auth(token)
        .send()
        .await
        .expect("start upload failed")
}

/// A `trg_` API token for `username`, created through the admin API.
async fn api_token(base_url: &str, username: &str) -> String {
    let resp = Client::new()
        .post(format!("{base_url}/api/v1/users/{username}/tokens"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "name": "ci" }))
        .send()
        .await
        .expect("create token request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = resp.json().await.expect("invalid json");
    body["token"].as_str().expect("token").to_string()
}

/// The registry token bought with `authorization` pings and starts an upload
/// on `oci-private/app`.
async fn assert_token_pushes(base_url: &str, authorization: &str) {
    let token = token(base_url, Some(authorization), "").await;
    assert_eq!(ping(base_url, &token).await.status(), StatusCode::OK);
    let resp = start_upload(base_url, "oci-private/app", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "{:?}",
        resp.text().await
    );
}

/// A `trg_` API token and its id, created through the admin API.
async fn api_token_with_id(base_url: &str, username: &str) -> (String, String) {
    let resp = Client::new()
        .post(format!("{base_url}/api/v1/users/{username}/tokens"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "name": "ci-revocable" }))
        .send()
        .await
        .expect("create token request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = resp.json().await.expect("invalid json");
    (
        body["id"].as_str().expect("id").to_string(),
        body["token"].as_str().expect("token").to_string(),
    )
}

#[tokio::test]
async fn registry_token_is_refused_outside_v2() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let authorization = basic_auth_header(USER, PASSWORD);
    let registry_token = token(&s.base_url, Some(&authorization), "").await;
    assert_eq!(ping(&s.base_url, &registry_token).await.status(), StatusCode::OK);
    for path in ["/api/v1/users", "/api/v1/me/permissions", "/npm-private/@acme/x"] {
        let resp = Client::new()
            .get(format!("{}{path}", s.base_url))
            .bearer_auth(&registry_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn registry_token_dies_with_the_api_token_it_was_bought_with() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let (id, api) = api_token_with_id(&s.base_url, USER).await;
    let registry_token = token(&s.base_url, Some(&format!("Bearer {api}")), "").await;
    assert_eq!(
        start_upload(&s.base_url, "oci-private/app", &registry_token).await.status(),
        StatusCode::ACCEPTED
    );
    let resp = Client::new()
        .delete(format!("{}/api/v1/users/{USER}/tokens/{id}", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.status());
    assert_eq!(ping(&s.base_url, &registry_token).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        start_upload(&s.base_url, "oci-private/app", &registry_token).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn v2_without_slash_is_a_json_404_and_private_ping_carries_api_version() {
    let s = spawn(false).await;
    let resp = reqwest::get(format!("{}/v2", s.base_url)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(header(&resp, "content-type").starts_with("application/json"));
    let resp = reqwest::get(format!("{}/v2/", s.base_url)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header(&resp, "docker-distribution-api-version"), "registry/2.0");
    assert_eq!(header(&resp, "www-authenticate"), realm(&s.base_url));
}

#[tokio::test]
async fn anonymous_ping_is_a_bearer_challenge() {
    let s = spawn(true).await;
    let resp = reqwest::get(format!("{}/v2/", s.base_url)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header(&resp, "www-authenticate"), realm(&s.base_url));
    assert_eq!(
        header(&resp, "docker-distribution-api-version"),
        "registry/2.0"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "UNAUTHORIZED");

    let resp = ping(&s.base_url, STATIC_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::OK, "an API token pings");
    let resp = Client::new()
        .get(format!("{}/v2/", s.base_url))
        .header("Authorization", basic_auth_header("admin", "nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header(&resp, "www-authenticate"), realm(&s.base_url));
}

#[tokio::test]
async fn anonymous_token_pings_and_pulls_a_public_image() {
    let s = spawn(true).await;
    seed_image(&s.base_url, "oci-public/team/app").await;
    let scope = "&scope=repository%3Aoci-public%2Fteam%2Fapp%3Apull";
    let token = token(&s.base_url, None, scope).await;

    assert_eq!(ping(&s.base_url, &token).await.status(), StatusCode::OK);
    let resp = Client::new()
        .get(format!(
            "{}/v2/oci-public/team/app/manifests/1.0",
            s.base_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "anonymous pull of a public image"
    );

    let resp = start_upload(&s.base_url, "oci-public/team/app", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an anonymous token never pushes"
    );
    assert_eq!(
        header(&resp, "www-authenticate"),
        format!(
            "{},scope=\"repository:oci-public/team/app:push\"",
            realm(&s.base_url)
        )
    );
    let resp = Client::new()
        .get(format!("{}/v2/oci-private/app/manifests/1.0", s.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "nor reads a private one"
    );
}

#[tokio::test]
async fn basic_credentials_buy_a_token_that_pushes() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let basic = basic_auth_header(USER, PASSWORD);
    let scope =
        "&scope=repository%3Aoci-private%2Fapp%3Apull%2Cpush&scope=repository%3Aother%2Fx%3Apull";
    let token = token(&s.base_url, Some(&basic), scope).await;

    assert_eq!(ping(&s.base_url, &token).await.status(), StatusCode::OK);
    let resp = start_upload(&s.base_url, "oci-private/app", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "{:?}",
        resp.text().await
    );

    let resp = start_upload(&s.base_url, "oci-private/app", "ocr_not-a-token").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = start_upload(&s.base_url, "oci-private/app", &token[..token.len() - 2]).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "a tampered mac");
    let (payload, mac) = token.split_once('.').unwrap();
    let forged = format!("{}.{mac}", &payload[..payload.len() - 1]);
    let resp = start_upload(&s.base_url, "oci-private/app", &forged).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a tampered payload"
    );
    assert_eq!(
        header(&resp, "www-authenticate"),
        format!(
            "{},scope=\"repository:oci-private/app:push\"",
            realm(&s.base_url)
        )
    );
}

/// `docker login -u user -p <scoped token>` is a Basic password, and the
/// provenance has to survive it: the bought token carries the scope of the
/// credential that paid for it, and dies with it.
#[tokio::test]
async fn a_scoped_token_presented_as_a_password_buys_a_token_that_keeps_its_scope() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let client = Client::new();
    let issued: Value = client
        .post(format!("{}/api/v1/users/{USER}/tokens", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({
            "name": "robot",
            "scope": {
                "kind": "limited",
                "grants": [{
                    "on": "repo",
                    "repo": "oci-private",
                    "actions": ["read", "write"]
                }]
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let scoped = issued["token"].as_str().expect("a token");
    let id = issued["id"].as_str().expect("an id").to_string();

    let bought = token(&s.base_url, Some(&basic_auth_header(USER, scoped)), "").await;
    assert_eq!(
        start_upload(&s.base_url, "oci-private/app", &bought)
            .await
            .status(),
        StatusCode::ACCEPTED,
        "in scope"
    );
    assert_eq!(
        start_upload(&s.base_url, "oci-public/app", &bought)
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "out of scope, and never a push it could not make with the credential"
    );

    let removed = client
        .delete(format!(
            "{}/api/v1/users/{USER}/tokens/{id}",
            s.base_url
        ))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    assert_eq!(
        start_upload(&s.base_url, "oci-private/app", &bought)
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "the bought token dies with the credential that paid for it"
    );
}

#[tokio::test]
async fn wrong_credentials_get_no_token() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let wrong = basic_auth_header(USER, "wrong-password");
    let resp = token_response(&s.base_url, Some(&wrong), "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().get("www-authenticate").is_none(),
        "the realm never challenges back to itself"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "UNAUTHORIZED");
    assert_eq!(body["errors"][0]["message"], "invalid credentials");
    let unknown = basic_auth_header("nobody", PASSWORD);
    let resp = token_response(&s.base_url, Some(&unknown), "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = token_response(&s.base_url, Some("Bearer trg_not-an-api-token"), "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_and_static_tokens_buy_a_token_that_pushes() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let api = api_token(&s.base_url, USER).await;
    assert_token_pushes(&s.base_url, &format!("Bearer {api}")).await;
    assert_token_pushes(&s.base_url, &format!("Bearer {STATIC_TOKEN}")).await;
}

#[tokio::test]
async fn repeated_wrong_passwords_are_throttled_like_basic_auth() {
    let s = spawn(true).await;
    publisher(&s.base_url).await;
    let wrong = basic_auth_header(USER, "wrong-password");
    for _ in 0..5 {
        let resp = token_response(&s.base_url, Some(&wrong), "").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = token_response(&s.base_url, Some(&wrong), "").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let right = basic_auth_header(USER, PASSWORD);
    let resp = token_response(&s.base_url, Some(&right), "").await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "one budget per username"
    );
}

#[tokio::test]
async fn no_anonymous_token_when_anonymous_read_is_off() {
    let s = spawn(false).await;
    publisher(&s.base_url).await;
    let resp = token_response(&s.base_url, None, "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let basic = basic_auth_header(USER, PASSWORD);
    let token = token(&s.base_url, Some(&basic), "").await;
    assert_eq!(ping(&s.base_url, &token).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_user_who_must_change_password_gets_a_token_but_no_access() {
    let s = spawn(true).await;
    let admin_password = std::fs::read_to_string(s.tmp.path().join("admin.password"))
        .expect("the initial admin password file");
    let basic = basic_auth_header("admin", admin_password.trim());
    let token = token(&s.base_url, Some(&basic), "").await;
    let resp = start_upload(&s.base_url, "oci-private/app", &token).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        ping(&s.base_url, &token).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_private_manifest_challenge_carries_the_pull_scope() {
    let s = spawn(true).await;
    let resp = reqwest::get(format!(
        "{}/v2/oci-private/team/app/manifests/1.0",
        s.base_url
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        header(&resp, "www-authenticate"),
        format!(
            "{},scope=\"repository:oci-private/team/app:pull\"",
            realm(&s.base_url)
        )
    );
    let resp = Client::new()
        .head(format!(
            "{}/v2/oci-private/app/blobs/{}",
            s.base_url,
            sha256_digest(b"x")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        header(&resp, "www-authenticate"),
        format!(
            "{},scope=\"repository:oci-private/app:pull\"",
            realm(&s.base_url)
        )
    );

    let closed = spawn(false).await;
    let resp = reqwest::get(format!("{}/v2/oci-public/app/tags/list", closed.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        header(&resp, "www-authenticate"),
        format!(
            "{},scope=\"repository:oci-public/app:pull\"",
            realm(&closed.base_url)
        )
    );
}
