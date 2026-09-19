//! Scoped tokens over HTTP: what a narrowed credential may do, and what the
//! same bearer does without it. Every clause here is about the difference the
//! scope makes, so each one issues both credentials from the same account.

mod common;

use base64::Engine;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use opencargo::config::{
    AdminConfig, AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat,
    RepositoryType, ServerConfig, Visibility,
};
use opencargo::server;

const ADMIN: &str = "test-token";

struct Server {
    url: String,
    client: reqwest::Client,
    _handle: tokio::task::JoinHandle<()>,
    _tmp: TempDir,
}

async fn setup() -> Server {
    let tmp = TempDir::new().unwrap();
    let mut config = Config {
        server: ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            base_url: "http://127.0.0.1:0".to_string(),
            storage_path: tmp.path().join("storage").to_str().unwrap().to_string(),
            ..Default::default()
        },
        database: DatabaseConfig {
            url: format!("sqlite:{}?mode=rwc", tmp.path().join("t.db").display()),
        },
        auth: AuthConfig {
            anonymous_read: true,
            static_tokens: vec![ADMIN.to_string()],
            admin: AdminConfig {
                username: "admin".to_string(),
                password: String::new(),
            },
            ..Default::default()
        },
        repositories: vec![
            private("npm-dev"),
            private("npm-prod"),
            RepositoryConfig {
                name: "npm-all".to_string(),
                repo_type: RepositoryType::Group,
                format: RepositoryFormat::Npm,
                visibility: Visibility::Private,
                members: Some(vec!["npm-dev".to_string()]),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    config.server.base_url = url.clone();
    let state = common::build_state(&mut config).await.unwrap();
    let router = server::build_router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client.get(format!("{url}/health/live")).send().await {
            Ok(r) if r.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    Server {
        url,
        client,
        _handle: handle,
        _tmp: tmp,
    }
}

fn private(name: &str) -> RepositoryConfig {
    RepositoryConfig {
        name: name.to_string(),
        repo_type: RepositoryType::Hosted,
        format: RepositoryFormat::Npm,
        visibility: Visibility::Private,
        ..Default::default()
    }
}

impl Server {
    async fn account(&self, username: &str, role: &str) {
        let r = self
            .client
            .post(format!("{}/api/v1/users", self.url))
            .bearer_auth(ADMIN)
            .json(&json!({ "username": username, "role": role }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "{:?}", r.text().await);
    }

    /// A credential of `username`, narrowed by `scope` when one is given.
    async fn token(&self, username: &str, name: &str, scope: Option<Value>) -> String {
        let mut body = json!({ "name": name });
        if let Some(scope) = scope {
            body["scope"] = scope;
        }
        let r = self
            .client
            .post(format!("{}/api/v1/users/{username}/tokens", self.url))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        body["token"].as_str().unwrap().to_string()
    }

    async fn publish(&self, repo: &str, package: &str, token: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .put(format!("{}/{repo}/{package}", self.url))
            .json(&packument(package, "1.0.0"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }

    async fn read(&self, repo: &str, package: &str, token: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .get(format!("{}/{repo}/{package}", self.url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }

    async fn set_tag(&self, repo: &str, package: &str, token: &str) -> reqwest::Response {
        self.client
            .put(format!(
                "{}/{repo}/-/package/{package}/dist-tags/next",
                self.url
            ))
            .bearer_auth(token)
            .json(&"1.0.0")
            .send()
            .await
            .unwrap()
    }

    async fn clear_tag(&self, repo: &str, package: &str, token: &str) -> reqwest::Response {
        self.client
            .delete(format!(
                "{}/{repo}/-/package/{package}/dist-tags/next",
                self.url
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }
}

fn repo_scope(pattern: &str, actions: &[&str]) -> Value {
    json!({
        "kind": "limited",
        "grants": [{ "on": "repo", "repo": pattern, "actions": actions }],
    })
}

fn packument(name: &str, version: &str) -> Value {
    let tarball = build_tarball();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&tarball);
    json!({
        "name": name,
        "dist-tags": { "latest": version },
        "versions": {
            version: {
                "name": name,
                "version": version,
                "dist": { "shasum": "" }
            }
        },
        "_attachments": {
            format!("{name}-{version}.tgz"): {
                "content_type": "application/octet-stream",
                "data": b64,
                "length": tarball.len()
            }
        }
    })
}

fn build_tarball() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = br#"{"name":"widget","version":"1.0.0"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_path("package/package.json").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &content[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    buf
}

async fn body(response: reqwest::Response) -> Value {
    response.json().await.unwrap()
}

/// The scope is what refuses: the same bearer, unscoped, publishes to both.
#[tokio::test]
async fn a_scoped_token_writes_where_it_is_scoped_and_nowhere_else() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let scoped = s
        .token("ci", "robot", Some(repo_scope("npm-dev", &["read", "write"])))
        .await;
    let full = s.token("ci", "session", None).await;

    assert_eq!(
        s.publish("npm-dev", "widget", Some(&scoped)).await.status(),
        StatusCode::OK
    );

    let refused = s.publish("npm-prod", "widget", Some(&scoped)).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    assert_eq!(
        s.publish("npm-prod", "widget", Some(&full)).await.status(),
        StatusCode::OK,
        "the bearer holds the right the scope removed"
    );
}

/// Reading obeys the same line, and the two refusals are not confused: no
/// credential is a 401 the client answers by presenting one, a credential out
/// of scope is a 403 it cannot answer at all.
#[tokio::test]
async fn reading_out_of_scope_is_a_403_and_reading_anonymously_is_a_401() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let scoped = s
        .token("ci", "robot", Some(repo_scope("npm-dev", &["read", "write"])))
        .await;
    let full = s.token("ci", "session", None).await;
    s.publish("npm-dev", "widget", Some(&full)).await;
    s.publish("npm-prod", "widget", Some(&full)).await;

    assert_eq!(
        s.read("npm-dev", "widget", Some(&scoped)).await.status(),
        StatusCode::OK
    );

    let out = s.read("npm-prod", "widget", Some(&scoped)).await;
    assert_eq!(out.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(out).await["code"], "insufficient_scope");

    let anonymous = s.read("npm-prod", "widget", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(body(anonymous).await["code"].is_null(), "no scope refused it");
}

/// `delete` is a rung of its own now: publishing does not carry removing.
#[tokio::test]
async fn a_write_scope_without_delete_publishes_and_removes_nothing() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let writer = s
        .token("ci", "writer", Some(repo_scope("npm-dev", &["read", "write"])))
        .await;
    let remover = s
        .token(
            "ci",
            "remover",
            Some(repo_scope("npm-dev", &["read", "write", "delete"])),
        )
        .await;
    s.publish("npm-dev", "widget", Some(&writer)).await;

    assert_eq!(
        s.set_tag("npm-dev", "widget", &writer).await.status(),
        StatusCode::OK
    );
    let refused = s.clear_tag("npm-dev", "widget", &writer).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    assert_eq!(
        s.clear_tag("npm-dev", "widget", &remover).await.status(),
        StatusCode::OK
    );
}

/// Invariant 4: whatever the target, including its own account, a scoped
/// credential makes no other credential.
#[tokio::test]
async fn a_scoped_token_issues_no_credential_even_for_its_own_account() {
    let s = setup().await;
    s.account("ci", "admin").await;
    let scoped = s
        .token("ci", "robot", Some(repo_scope("npm-*", &["read"])))
        .await;

    for target in ["ci", "admin"] {
        let refused = s
            .client
            .post(format!("{}/api/v1/users/{target}/tokens", s.url))
            .bearer_auth(&scoped)
            .json(&json!({ "name": "escalation" }))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{target}");
    }
}

/// A scope holds incarnations, not names: a repository created after the
/// token was issued is outside it, whatever the pattern says.
#[tokio::test]
async fn a_pattern_never_reaches_a_repository_created_after_the_token() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let scoped = s
        .token("ci", "robot", Some(repo_scope("npm-*", &["read", "write"])))
        .await;

    let created = s
        .client
        .post(format!("{}/api/v1/repositories", s.url))
        .bearer_auth(ADMIN)
        .json(&json!({
            "name": "npm-later",
            "type": "hosted",
            "format": "npm",
            "visibility": "private"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED, "{:?}", created.text().await);

    assert_eq!(
        s.publish("npm-dev", "widget", Some(&scoped)).await.status(),
        StatusCode::OK
    );
    let refused = s.publish("npm-later", "widget", Some(&scoped)).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");
}

/// Revoking is the only way a scope changes, and it is a fact at the next
/// request, not at the next refresh.
#[tokio::test]
async fn revoking_a_scoped_token_refuses_it_on_the_next_request() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let scoped = s
        .token("ci", "robot", Some(repo_scope("npm-dev", &["read", "write"])))
        .await;
    assert_eq!(
        s.publish("npm-dev", "widget", Some(&scoped)).await.status(),
        StatusCode::OK
    );

    let listed: Value = s
        .client
        .get(format!("{}/api/v1/users/ci/tokens", s.url))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = listed[0]["id"].as_str().unwrap().to_string();
    assert_eq!(listed[0]["scope"]["kind"], "limited", "the listing shows it");

    let removed = s
        .client
        .delete(format!("{}/api/v1/users/ci/tokens/{id}", s.url))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);

    assert_eq!(
        s.read("npm-dev", "widget", Some(&scoped)).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

/// A group is judged at its entrance, on the name the client asked for: a
/// scope that names a member and not the group is refused there, loudly,
/// rather than answered with an index missing what it may not see.
#[tokio::test]
async fn a_group_is_judged_on_the_name_the_client_asked_for() {
    let s = setup().await;
    s.account("ci", "publisher").await;
    let full = s.token("ci", "session", None).await;
    s.publish("npm-dev", "widget", Some(&full)).await;

    let member_only = s
        .token("ci", "member", Some(repo_scope("npm-dev", &["read"])))
        .await;
    let refused = s.read("npm-all", "widget", Some(&member_only)).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN, "never a 404");
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    let group = s
        .token("ci", "group", Some(repo_scope("npm-all", &["read"])))
        .await;
    let served = s.read("npm-all", "widget", Some(&group)).await;
    assert_eq!(
        served.status(),
        StatusCode::OK,
        "the group serves what its members hold"
    );
}

/// The administrative surface asks for a credential that is not narrowed:
/// the same bearer, unscoped, does every one of these.
#[tokio::test]
async fn a_scoped_token_reaches_no_administrative_route() {
    let s = setup().await;
    s.account("root", "admin").await;
    let scoped = s
        .token("root", "robot", Some(repo_scope("npm-*", &["read", "write"])))
        .await;
    let full = s.token("root", "session", None).await;

    let flip = json!({ "visibility": "public" });
    let refused = s
        .client
        .put(format!("{}/api/v1/repositories/npm-prod", s.url))
        .bearer_auth(&scoped)
        .json(&flip)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    for (method, path) in [
        ("DELETE", "/api/v1/repositories/npm-prod"),
        ("GET", "/api/v1/system/audit"),
        ("GET", "/api/v1/system/storage"),
        ("GET", "/api/v1/users"),
    ] {
        let request = match method {
            "DELETE" => s.client.delete(format!("{}{path}", s.url)),
            _ => s.client.get(format!("{}{path}", s.url)),
        };
        let refused = request.bearer_auth(&scoped).send().await.unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }

    let allowed = s
        .client
        .put(format!("{}/api/v1/repositories/npm-prod", s.url))
        .bearer_auth(&full)
        .json(&flip)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK, "the scope is what refused");
}

/// The instance report is administrative like the rest: an admin's scoped
/// token does not read it, the same admin's session does.
#[tokio::test]
async fn a_scoped_admin_token_does_not_read_the_instance_report() {
    let s = setup().await;
    s.account("root", "admin").await;
    let scoped = s
        .token("root", "robot", Some(repo_scope("npm-*", &["read", "write"])))
        .await;
    let full = s.token("root", "session", None).await;
    let url = format!("{}/api/v1/system/instance", s.url);

    let refused = s.client.get(&url).bearer_auth(&scoped).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    let served = s.client.get(&url).bearer_auth(&full).send().await.unwrap();
    assert_eq!(served.status(), StatusCode::OK);
    assert!(body(served).await["owner"].is_string());
}

/// Promotion asks for the repository action `admin`, on both repositories,
/// and a scope that does not name it does not promote.
#[tokio::test]
async fn promotion_asks_for_the_admin_rung_on_both_repositories() {
    let s = setup().await;
    s.account("root", "admin").await;
    let full = s.token("root", "session", None).await;
    s.publish("npm-dev", "widget", Some(&full)).await;

    let writer = s
        .token(
            "root",
            "writer",
            Some(repo_scope("npm-*", &["read", "write"])),
        )
        .await;
    let promoter = s
        .token(
            "root",
            "promoter",
            Some(repo_scope("npm-*", &["read", "write", "admin"])),
        )
        .await;
    let promote = json!({ "from": "npm-dev", "to": "npm-prod" });

    let refused = s
        .client
        .post(format!("{}/api/v1/promote/widget/1.0.0", s.url))
        .bearer_auth(&writer)
        .json(&promote)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(refused).await["code"], "insufficient_scope");

    let done = s
        .client
        .post(format!("{}/api/v1/promote/widget/1.0.0", s.url))
        .bearer_auth(&promoter)
        .json(&promote)
        .send()
        .await
        .unwrap();
    assert_eq!(done.status(), StatusCode::OK, "{:?}", done.text().await);
}

/// The vocabulary is closed at creation, and a subscription no repository
/// selector can narrow is not part of it.
#[tokio::test]
async fn a_scope_outside_the_vocabulary_is_refused_at_creation() {
    let s = setup().await;
    s.account("ci", "publisher").await;

    for scope in [
        json!({ "kind": "limited", "grants": [{ "on": "admin", "domain": "webhooks", "actions": ["write"] }] }),
        json!({ "kind": "limited", "grants": [{ "on": "admin", "domain": "tokens", "actions": ["write"] }] }),
        json!({ "kind": "limited", "grants": [{ "on": "repo", "repo": "npm-dev", "actions": ["teleport"] }] }),
        json!({ "kind": "wide-open" }),
    ] {
        let refused = s
            .client
            .post(format!("{}/api/v1/users/ci/tokens", s.url))
            .bearer_auth(ADMIN)
            .json(&json!({ "name": "bad", "scope": scope }))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{scope}");
    }
}
