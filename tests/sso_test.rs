//! SSO over HTTP against a real server and a fake OpenID Provider: the
//! cookie's attributes, the handoff's single use, the refusal of a
//! cross-site form, the startup checks, and credentials that die with the
//! account they came from.

mod common;

use common::fake_idp::{self, FakeIdp, CLIENT_ID, CLIENT_SECRET};
use common::{basic_auth_header, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{Config, SsoConfig, SsoProviderConfig};
use reqwest::{redirect, Client, StatusCode};
use serde_json::{json, Value};

const PUBLIC: &str = "https://registry.example";

fn provider(idp: &FakeIdp) -> SsoProviderConfig {
    SsoProviderConfig {
        name: "corp".into(),
        kind: "generic".into(),
        issuer: idp.issuer.clone(),
        client_id: CLIENT_ID.into(),
        client_secret: CLIENT_SECRET.into(),
        groups_claim: Some("groups".into()),
        open: Some(false),
        ..Default::default()
    }
}

fn sso(idp: &FakeIdp) -> SsoConfig {
    SsoConfig {
        providers: vec![provider(idp)],
        ..Default::default()
    }
}

async fn spawn(idp: &FakeIdp) -> TestServer {
    spawn_server(SpawnOpts {
        sso: sso(idp),
        public_url: Some(PUBLIC.into()),
        ..Default::default()
    })
    .await
}

fn no_redirects() -> Client {
    Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap()
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .to_str()
        .unwrap()
        .to_string()
}

/// `name=value` of a `Set-Cookie` line.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_string()
}

struct Landing {
    location: String,
    cookie: String,
    set_cookie: String,
}

/// Start, let the IdP approve, and come back to our callback: what a
/// browser does before the SPA takes over.
async fn land(server: &TestServer) -> Landing {
    let client = no_redirects();
    let start = client
        .get(format!(
            "{}/api/v1/auth/sso/corp/start?return_to=/packages",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    let set_cookie = header(&start, "set-cookie");
    let cookie = cookie_pair(&set_cookie);
    let approved = client.get(header(&start, "location")).send().await.unwrap();
    let back = header(&approved, "location");
    let back = back.replacen(PUBLIC, &server.base_url, 1);
    let landed = client
        .get(back)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(landed.status(), StatusCode::SEE_OTHER);
    Landing {
        location: header(&landed, "location"),
        cookie,
        set_cookie,
    }
}

fn code(landing: &Landing) -> String {
    landing
        .location
        .split("code=")
        .nth(1)
        .expect("a code")
        .to_string()
}

async fn exchange(server: &TestServer, code: &str, cookie: Option<&str>) -> reqwest::Response {
    let mut req = Client::new()
        .post(format!("{}/api/v1/auth/sso/exchange", server.base_url))
        .json(&json!({ "code": code }));
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    req.send().await.unwrap()
}

async fn login(server: &TestServer) -> Value {
    let landing = land(server).await;
    let resp = exchange(server, &code(&landing), Some(&landing.cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn the_attempt_cookie_is_host_prefixed_secure_http_only_and_lax() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let landing = land(&server).await;
    let c = &landing.set_cookie;
    assert!(c.starts_with("__Host-oc_sso="), "{c}");
    for attr in ["Path=/", "HttpOnly", "SameSite=Lax", "Secure"] {
        assert!(c.contains(attr), "{attr} in {c}");
    }
    assert!(!c.to_ascii_lowercase().contains("domain="), "{c}");
    assert!(landing.location.starts_with("/login/sso/complete#code="));
    assert!(!landing.location.contains("eyJ"), "no ID Token in a URL");
}

#[tokio::test]
async fn a_handoff_needs_its_cookie_and_is_used_once() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let landing = land(&server).await;
    assert_eq!(
        exchange(&server, &code(&landing), None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        exchange(&server, &code(&landing), Some(&landing.cookie))
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "the refused exchange consumed it"
    );

    let landing = land(&server).await;
    let ok = exchange(&server, &code(&landing), Some(&landing.cookie)).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert!(header(&ok, "set-cookie").contains("Max-Age=0"));
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["username"], "dev");
    assert_eq!(body["return_to"], "/packages");
    assert_eq!(
        exchange(&server, &code(&landing), Some(&landing.cookie))
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "a replay is refused"
    );
}

#[tokio::test]
async fn a_cross_site_form_cannot_exchange() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let landing = land(&server).await;
    let resp = Client::new()
        .post(format!("{}/api/v1/auth/sso/exchange", server.base_url))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", &landing.cookie)
        .body(format!("code={}", code(&landing)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        exchange(&server, &code(&landing), Some(&landing.cookie))
            .await
            .status(),
        StatusCode::OK,
        "the refused form consumed nothing"
    );
}

#[tokio::test]
async fn a_forged_callback_is_refused_and_writes_no_account() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let resp = no_redirects()
        .get(format!(
            "{}/api/v1/auth/sso/corp/callback?code=x&state=y",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(header(&resp, "location"), "/login?sso_error=state_mismatch");
    assert_eq!(idp.token_hits(), 0);
    let users: Value = Client::new()
        .get(format!("{}/api/v1/users", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(users
        .as_array()
        .unwrap()
        .iter()
        .all(|u| u["username"] != "dev"));
}

#[tokio::test]
async fn the_session_token_works_as_bearer_and_as_basic_password() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let session = login(&server).await;
    let token = session["token"].as_str().unwrap();
    for auth in [
        format!("Bearer {token}"),
        basic_auth_header("anyone", token),
    ] {
        let who: Value = Client::new()
            .get(format!("{}/-/whoami", server.base_url))
            .header("authorization", auth)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(who["username"], "dev");
    }
}

#[tokio::test]
async fn an_oci_token_is_refused_once_its_account_is_disabled() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let session = login(&server).await;
    let token = session["token"].as_str().unwrap();
    let issued: Value = Client::new()
        .get(format!("{}/v2/token?service=opencargo", server.base_url))
        .header("authorization", basic_auth_header("dev", token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let registry = issued["token"].as_str().unwrap().to_string();
    let ping = |t: String| {
        let url = format!("{}/v2/", server.base_url);
        async move {
            Client::new()
                .get(url)
                .bearer_auth(t)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(ping(registry.clone()).await, StatusCode::OK);
    let disabled = Client::new()
        .post(format!("{}/api/v1/users/dev/disable", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::NO_CONTENT);
    assert_eq!(ping(registry).await, StatusCode::UNAUTHORIZED);
    assert_eq!(ping(token.to_string()).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_role_of_an_sso_account_is_not_the_admins_to_change() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    login(&server).await;
    let resp = Client::new()
        .put(format!("{}/api/v1/users/dev", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({"role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

fn config_with(base_url: &str, bind: &str, dev: bool) -> Config {
    let mut config = Config::default();
    config.server.base_url = base_url.into();
    config.server.bind = bind.into();
    config.auth.sso.dev_insecure_http = dev;
    config.auth.sso.providers = vec![SsoProviderConfig {
        name: "corp".into(),
        kind: "generic".into(),
        issuer: "https://idp.example".into(),
        ..Default::default()
    }];
    config
}

#[test]
fn the_dev_flag_is_refused_with_an_https_url_or_a_non_loopback_bind() {
    use opencargo::server::check_sso_transport;
    assert!(check_sso_transport(&config_with(
        "http://127.0.0.1:6789",
        "127.0.0.1:6789",
        true
    ))
    .is_ok());
    assert!(check_sso_transport(&config_with("http://localhost:6789", "[::1]:6789", true)).is_ok());
    assert!(
        check_sso_transport(&config_with("https://r.example", "127.0.0.1:6789", true)).is_err()
    );
    assert!(
        check_sso_transport(&config_with("http://127.0.0.1:6789", "0.0.0.0:6789", true)).is_err()
    );
    assert!(
        check_sso_transport(&config_with("http://r.example", "0.0.0.0:6789", false)).is_err(),
        "without the flag, SSO needs https"
    );
    assert!(check_sso_transport(&config_with("https://r.example", "0.0.0.0:6789", false)).is_ok());
}

#[tokio::test]
async fn an_undeclared_missing_authority_refuses_the_start_and_a_retired_one_revokes() {
    let idp = fake_idp::start().await;
    let server = spawn(&idp).await;
    let session = login(&server).await;
    let token = session["token"].as_str().unwrap().to_string();

    let error = common::start_error_in(
        &server,
        SpawnOpts {
            public_url: Some(PUBLIC.into()),
            ..Default::default()
        },
    )
    .await;
    assert!(
        error.contains("corp") && error.contains("retired"),
        "{error}"
    );
    let who = Client::new()
        .get(format!("{}/-/whoami", server.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        who.status(),
        StatusCode::OK,
        "the refused start revoked nothing"
    );
    let mut retired = provider(&idp);
    retired.retired = true;
    let server = common::respawn(
        server,
        SpawnOpts {
            public_url: Some(PUBLIC.into()),
            sso: SsoConfig {
                providers: vec![retired],
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    let who = Client::new()
        .get(format!("{}/-/whoami", server.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        who.status(),
        StatusCode::UNAUTHORIZED,
        "the retired provider's session is revoked"
    );
}

/// A flood of callbacks whose code exchanges the IdP throttles never opens
/// an outage: only the server's own probe does.
#[tokio::test]
async fn forged_callback_flood_does_not_suspend_reauth_after() {
    let idp = fake_idp::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = config_with(PUBLIC, "127.0.0.1:0", false);
    config.auth.sso.providers = vec![provider(&idp)];
    config.auth.sso.reauth_after = "1h".into();
    config.server.storage_path = tmp.path().join("storage").display().to_string();
    config.database.url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
    let state = opencargo::server::build_state(&config).await.unwrap();
    idp.token_status(429);
    for _ in 0..40 {
        let begun = state.sso.begin_login("corp", "/").await.unwrap();
        let state_param = url::Url::parse(&begun.location)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        let callback = opencargo::ports::identity_provider::Callback {
            code: Some("forged".into()),
            state: Some(state_param),
            iss: Some(idp.issuer.clone()),
            error: None,
        };
        assert!(state
            .sso
            .complete("corp", &callback, Some(&begun.cookie), Default::default())
            .await
            .is_err());
    }
    assert!(idp.token_hits() >= 40);
    state.sso.probe_all().await;
    let corp = opencargo::domain::identity::Authority::new("corp", &idp.issuer);
    assert!(state.identities.outages(&corp).await.unwrap().is_empty());
    idp.discovery_status(503);
    state.sso.probe_all().await;
    assert_eq!(state.identities.outages(&corp).await.unwrap().len(), 1);
}
