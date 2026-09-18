use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::domain::identity::{Authority, Groups, LoginPolicy};
use crate::testing::fake_idp::{self, FakeIdp, CLIENT_ID, CLIENT_SECRET};

const REDIRECT: &str = "http://127.0.0.1:1/api/v1/auth/sso/corp/callback";

fn provider_for(_idp: &FakeIdp, kind: Kind, issuer: &str) -> OidcProvider {
    let settings = OidcSettings {
        name: "corp".into(),
        kind,
        issuer: issuer.to_string(),
        client_id: CLIENT_ID.into(),
        client_secret: CLIENT_SECRET.into(),
        extra_scopes: vec![],
        timeout: Duration::from_secs(5),
        jwks_refresh_floor: Duration::ZERO,
    };
    let profile = ProviderProfile {
        authority: Authority::new("corp", issuer),
        open: false,
        allow_open: false,
        tenant_pinned: false,
        authoritative_domains: vec![],
        policy: LoginPolicy {
            default_role: "reader".into(),
            ..LoginPolicy::default()
        },
    };
    OidcProvider::new(settings, profile).unwrap()
}

fn provider(idp: &FakeIdp) -> OidcProvider {
    provider_for(
        idp,
        Kind::Generic {
            groups_claim: Some("groups".into()),
            open: false,
        },
        &idp.issuer,
    )
}

fn params(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// What the browser would bring back: the authorization endpoint's code for
/// this attempt, with the `iss` it adds.
async fn approve(p: &OidcProvider, idp: &FakeIdp, iss: Option<&str>) -> (Attempt, Callback) {
    let (url, attempt) = p.start(REDIRECT).await.unwrap();
    let q = params(&url);
    let code = idp.issue_code(&q["nonce"], &q["code_challenge"], REDIRECT);
    let callback = Callback {
        code: Some(code),
        state: Some(q["state"].clone()),
        iss: iss.map(str::to_string),
        error: None,
    };
    (attempt, callback)
}

#[tokio::test]
async fn the_code_flow_uses_pkce_s256_a_nonce_and_query_mode() {
    let idp = fake_idp::start().await;
    idp.login_as(json!({"sub": "u1", "groups": ["dev"], "preferred_username": "Dev"}));
    let p = provider(&idp);
    let (url, attempt) = p.start(REDIRECT).await.unwrap();
    let q = params(&url);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["response_mode"], "query");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"], challenge(&attempt.verifier));
    assert_eq!(q["state"], attempt.state);
    assert_eq!(q["nonce"], attempt.nonce);

    let (attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    let id = p.finish(&attempt, &callback, REDIRECT).await.unwrap();
    assert_eq!(id.key.subject, "u1");
    assert_eq!(id.key.authority, Authority::new("corp", &idp.issuer));
    assert_eq!(id.groups, Groups::Listed(vec!["dev".into()]));
    assert_eq!(id.preferred_name.as_deref(), Some("Dev"));
}

#[tokio::test]
async fn a_wrong_verifier_or_nonce_is_refused() {
    let idp = fake_idp::start().await;
    let p = provider(&idp);
    let (mut attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    attempt.verifier = random_token();
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::IdpError(_))
    ));
    let (mut attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    attempt.nonce = random_token();
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::Rejected(_))
    ));
}

#[tokio::test]
async fn the_iss_parameter_is_required_if_and_only_if_announced() {
    let idp = fake_idp::start().await;
    let p = provider(&idp);
    let (attempt, callback) = approve(&p, &idp, None).await;
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::Rejected(_))
    ));
    let (attempt, callback) = approve(&p, &idp, Some("https://evil.example")).await;
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::Rejected(_))
    ));

    let idp = fake_idp::start().await;
    idp.iss_parameter(false, false);
    let p = provider(&idp);
    let (attempt, callback) = approve(&p, &idp, None).await;
    assert!(p.finish(&attempt, &callback, REDIRECT).await.is_ok());
}

#[tokio::test]
async fn a_discovery_issuer_differing_by_more_than_a_slash_is_refused() {
    let idp = fake_idp::start().await;
    idp.set_discovery_issuer(&format!("{}/", idp.issuer));
    assert!(provider(&idp).start(REDIRECT).await.is_ok());
    let idp = fake_idp::start().await;
    idp.set_discovery_issuer("https://other.example");
    assert!(matches!(
        provider(&idp).start(REDIRECT).await,
        Err(IdpFailure::Rejected(_))
    ));
}

#[tokio::test]
async fn a_rotated_key_is_fetched_once_it_signs() {
    let idp = fake_idp::start().await;
    let p = provider(&idp);
    let (attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    p.finish(&attempt, &callback, REDIRECT).await.unwrap();
    let before = idp.jwks_hits();
    idp.rotate("k2");
    idp.retire_old_keys();
    let (attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    p.finish(&attempt, &callback, REDIRECT).await.unwrap();
    assert_eq!(idp.jwks_hits(), before + 1);
}

#[tokio::test]
async fn symmetric_and_unsigned_tokens_are_refused() {
    let idp = fake_idp::start().await;
    let p = provider(&idp);
    p.start(REDIRECT).await.unwrap();
    let claims = json!({"iss": idp.issuer, "aud": CLIENT_ID, "sub": "x", "nonce": "n",
                        "exp": chrono::Utc::now().timestamp() + 60});
    let hs = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(CLIENT_SECRET.as_bytes()),
    )
    .unwrap();
    assert!(matches!(
        p.verify(&hs, "n").await,
        Err(IdpFailure::Rejected(_))
    ));
    let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let none = format!("{}.{}.", b64(r#"{"alg":"none"}"#), b64(&claims.to_string()));
    assert!(p.verify(&none, "n").await.is_err());
    let signed = idp.sign(&claims);
    assert!(p.verify(&signed, "n").await.is_ok());
    let mut other = claims.clone();
    other["aud"] = json!("someone-else");
    assert!(p.verify(&idp.sign(&other), "n").await.is_err());
}

#[tokio::test]
async fn concurrent_starts_share_one_discovery_flight() {
    let idp = fake_idp::start().await;
    idp.discovery_delay(200);
    let p = Arc::new(provider(&idp));
    let calls: Vec<_> = (0..10)
        .map(|_| {
            let p = p.clone();
            tokio::spawn(async move { p.start(REDIRECT).await.is_ok() })
        })
        .collect();
    for call in calls {
        assert!(call.await.unwrap());
    }
    assert_eq!(idp.discovery_hits(), 1);
}

#[tokio::test]
async fn a_failed_discovery_is_cached_negatively_until_the_probe_sees_it_back() {
    let idp = fake_idp::start().await;
    idp.discovery_status(503);
    let p = provider(&idp);
    assert!(matches!(
        p.start(REDIRECT).await,
        Err(IdpFailure::Unavailable(_))
    ));
    idp.discovery_status(200);
    let hits = idp.discovery_hits();
    assert!(matches!(
        p.start(REDIRECT).await,
        Err(IdpFailure::Unavailable(_))
    ));
    assert_eq!(
        idp.discovery_hits(),
        hits,
        "the negative cache calls nobody"
    );
    assert!(p.probe().await);
    assert!(p.start(REDIRECT).await.is_ok());
    idp.jwks_status(503);
    assert!(!p.probe().await, "the probe covers the keys too");
}

#[tokio::test]
async fn a_throttled_code_exchange_is_an_idp_error_not_an_outage() {
    let idp = fake_idp::start().await;
    let p = provider(&idp);
    let (attempt, callback) = approve(&p, &idp, Some(&idp.issuer)).await;
    idp.token_status(429);
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::IdpError(_))
    ));
    idp.token_status(503);
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::Unavailable(_))
    ));
}

#[tokio::test]
async fn an_entra_tenant_profile_checks_the_tenant_issuer() {
    let idp = fake_idp::start().await;
    let template = format!("{}/{{tid}}/v2.0", idp.issuer);
    let concrete = format!("{}/t1/v2.0", idp.issuer);
    idp.set_discovery_issuer(&concrete);
    idp.set_token_iss(&concrete);
    idp.login_as(
        json!({"tid": "t1", "groups": ["g"], "email": "a@corp.example", "email_verified": false}),
    );
    let p = provider_for(
        &idp,
        Kind::Entra {
            tenant: "t1".into(),
        },
        &template,
    );
    assert!(p.profile().tenant_pinned);
    let (attempt, callback) = approve(&p, &idp, Some(&concrete)).await;
    let id = p.finish(&attempt, &callback, REDIRECT).await.unwrap();
    assert_eq!(
        id.email_trust,
        crate::domain::identity::EmailTrust::Verified
    );
    assert_eq!(id.tenant.as_deref(), Some("t1"));

    idp.login_as(json!({"tid": "t2"}));
    let (attempt, callback) = approve(&p, &idp, Some(&concrete)).await;
    assert!(matches!(
        p.finish(&attempt, &callback, REDIRECT).await,
        Err(IdpFailure::Rejected(_))
    ));
}
