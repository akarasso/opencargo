use super::*;
use crate::ports::tokens::NewToken;
use crate::ports::users::NewUser;
use crate::registry::oci::token::TokenSigner;
use crate::testing::fakes::FakeDb;

struct Fixed;

impl crate::ports::clock::Clock for Fixed {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Refuses whatever `refuse` names.
struct Gate {
    refuse: Vec<CredentialKind>,
}

impl LoginGate for Gate {
    fn login_allowed(&self, _user: &User, kind: CredentialKind, _now: DateTime<Utc>) -> bool {
        !self.refuse.contains(&kind)
    }
}

struct Fx {
    store: FakeDb,
    signer: Arc<TokenSigner>,
    auth: Authenticate,
}

const PASSWORD: &str = "correct horse";

async fn user(db: &FakeDb, name: &str) -> User {
    let hash = passwords::hash_password(PASSWORD).unwrap();
    db.users()
        .create(
            &NewUser {
                username: name,
                email: None,
                password_hash: &hash,
                role: "reader",
            },
            Utc::now(),
        )
        .await
        .unwrap()
}

async fn token_of(db: &FakeDb, user: &User, id: &str) -> String {
    let (raw, hash) = tokens::generate_token("trg_");
    db.tokens()
        .create(
            &NewToken {
                id,
                user_id: user.id,
                name: "ci",
                prefix: &raw[..16],
                token_hash: &hash,
                expires_at: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    raw
}

fn fixture_with(refuse: Vec<CredentialKind>) -> Fx {
    let db = FakeDb::new();
    let signer = Arc::new(TokenSigner::random());
    let auth = Authenticate::new(AuthenticateDeps {
        static_tokens: vec!["static-secret".to_string()],
        token_prefix: "trg_".to_string(),
        users: db.users(),
        tokens: db.tokens(),
        signer: signer.clone(),
        login_limiter: Arc::new(RateLimiter::new(3, 60)),
        token_limiter: Arc::new(RateLimiter::new(3, 60)),
        gate: Arc::new(Gate { refuse }),
        clock: Arc::new(Fixed),
    });
    Fx { store: db, signer, auth }
}

fn fixture() -> Fx {
    fixture_with(Vec::new())
}

fn basic(username: &str, password: &str) -> Presented {
    Presented {
        transport: Transport::Authorization,
        credential: Credential::Basic {
            username: username.to_string(),
            password: password.to_string(),
        },
    }
}

fn bearer(raw: &str) -> Presented {
    Presented {
        transport: Transport::Authorization,
        credential: Credential::Bearer(raw.to_string()),
    }
}

fn api_key(raw: &str) -> Presented {
    Presented {
        transport: Transport::ApiKeyHeader,
        credential: Credential::ApiKey(raw.to_string()),
    }
}

impl Fx {
    async fn who(&self, presented: &[Presented], source: &str) -> Result<Option<String>, Refusal> {
        self.auth
            .run(presented, None, source)
            .await
            .map(|d| d.and_then(|d| d.user).map(|u| u.username))
    }

    async fn lock_out(&self, name: &str) {
        for _ in 0..3 {
            let refused = self.who(&[basic(name, "wrong")], "10.0.0.1").await;
            assert_eq!(refused, Err(Refusal::Invalid));
        }
        assert_eq!(
            self.who(&[basic(name, PASSWORD)], "10.0.0.1").await,
            Err(Refusal::Throttled),
            "the account is locked for passwords"
        );
    }
}

#[tokio::test]
async fn api_token_as_basic_password_not_throttled_by_account_lockout() {
    let fx = fixture();
    let ci = user(&fx.store, "ci").await;
    let raw = token_of(&fx.store, &ci, "t1").await;
    fx.lock_out("ci").await;

    assert_eq!(fx.who(&[basic("ci", &raw)], "10.0.0.1").await, Ok(Some("ci".into())));
    assert_eq!(fx.who(&[basic("anything", &raw)], "10.0.0.1").await, Ok(Some("ci".into())));
    assert_eq!(fx.who(&[bearer(&raw)], "10.0.0.1").await, Ok(Some("ci".into())));
}

#[tokio::test]
async fn basic_and_npm_login_share_one_budget_and_count_failures_only() {
    let fx = fixture();
    user(&fx.store, "dev").await;
    for _ in 0..5 {
        assert!(fx.who(&[basic("dev", PASSWORD)], "s").await.is_ok());
    }
    assert_eq!(fx.auth.password("dev", "wrong").await.unwrap_err(), Refusal::Invalid);
    assert_eq!(fx.who(&[basic("dev", "wrong")], "s").await, Err(Refusal::Invalid));
    assert_eq!(fx.auth.password("DEV", "wrong").await.unwrap_err(), Refusal::Invalid);
    assert_eq!(
        fx.auth.password("dev", PASSWORD).await.unwrap_err(),
        Refusal::Throttled,
        "npm login and Basic spend one budget, whatever the case of the name"
    );
}

#[tokio::test]
async fn password_change_is_throttled() {
    use crate::app::audit::Actor;
    use crate::app::users::ChangePassword;
    let fx = fixture();
    user(&fx.store, "dev").await;
    fx.lock_out("dev").await;
    let by = Actor {
        user_id: Some(1),
        username: "dev",
        admin: false,
    };
    let refused = ChangePassword::new(fx.store.users(), Arc::new(fx.auth))
        .run("dev", Some(PASSWORD), "new password!", &by, Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, crate::error::AppError::TooManyRequests(_)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn invalid_tokens_throttle_their_source_only_and_never_login() {
    let fx = fixture();
    let dev = user(&fx.store, "dev").await;
    let raw = token_of(&fx.store, &dev, "t1").await;
    for _ in 0..3 {
        assert_eq!(fx.who(&[bearer("trg_0000000000000000junk")], "bad").await, Err(Refusal::Invalid));
    }
    assert_eq!(fx.who(&[bearer("junk")], "bad").await, Err(Refusal::Throttled));
    assert_eq!(
        fx.who(&[bearer(&raw)], "bad").await,
        Ok(Some("dev".into())),
        "a valid token is verified before the limiter"
    );
    assert_eq!(fx.who(&[bearer("junk")], "good").await, Err(Refusal::Invalid));
    assert_eq!(fx.who(&[basic("dev", PASSWORD)], "bad").await, Ok(Some("dev".into())));
}

#[tokio::test]
async fn junk_token_under_a_victims_name_does_not_lock_the_victim() {
    let fx = fixture();
    user(&fx.store, "victim").await;
    for _ in 0..10 {
        let _ = fx.who(&[basic("victim", "trg_notatokenatall")], "attacker").await;
    }
    assert_eq!(
        fx.who(&[basic("victim", PASSWORD)], "ci").await,
        Ok(Some("victim".into()))
    );
}

#[tokio::test]
async fn a_presented_credential_that_fails_is_never_anonymous() {
    let fx = fixture();
    assert_eq!(fx.who(&[bearer("nope")], "s").await, Err(Refusal::Invalid));
    assert_eq!(fx.who(&[], "s").await, Ok(None));
    assert_eq!(
        fx.who(&[bearer("static-secret")], "s").await,
        Ok(Some("static-token".into()))
    );
}

#[tokio::test]
async fn several_credentials_are_all_verified_and_the_primary_decides() {
    let fx = fixture();
    let alice = user(&fx.store, "alice").await;
    let bob = user(&fx.store, "bob").await;
    let a = token_of(&fx.store, &alice, "ta").await;
    let b = token_of(&fx.store, &bob, "tb").await;
    let both = [basic("alice", PASSWORD), api_key(&b)];

    let push = fx.auth.run(&both, Some(Transport::ApiKeyHeader), "s").await.unwrap();
    assert_eq!(push.unwrap().user.unwrap().username, "bob");
    assert_eq!(
        fx.auth.run(&both, None, "s").await.unwrap_err(),
        Refusal::Invalid,
        "no declared primary, two principals"
    );
    let same = [bearer(&a), basic("alice", PASSWORD)];
    assert_eq!(fx.who(&same, "s").await, Ok(Some("alice".into())));
    let one_bad = [basic("alice", PASSWORD), api_key("trg_0000000000000000bad")];
    assert_eq!(
        fx.auth.run(&one_bad, Some(Transport::Authorization), "s").await.unwrap_err(),
        Refusal::Invalid
    );
}

#[tokio::test]
async fn password_refused_by_the_gate_while_its_token_is_accepted() {
    let fx = fixture_with(vec![CredentialKind::Password]);
    let dev = user(&fx.store, "dev").await;
    let raw = token_of(&fx.store, &dev, "t1").await;
    assert_eq!(fx.who(&[basic("dev", PASSWORD)], "s").await, Err(Refusal::Invalid));
    assert_eq!(fx.who(&[basic("dev", &raw)], "s").await, Ok(Some("dev".into())));
}

/// The gate is evaluated after every verification, on every scheme: SSO's
/// `reauth_after` plugs in here.
#[tokio::test]
async fn api_and_oci_tokens_refused_when_login_is_not_allowed() {
    let refuse = vec![CredentialKind::ApiToken, CredentialKind::RegistryToken];
    let fx = fixture_with(refuse);
    let dev = user(&fx.store, "dev").await;
    let raw = token_of(&fx.store, &dev, "t1").await;
    let registry = fx.signer.sign(&Claims {
        sub: Some("dev".into()),
        exp: i64::MAX,
        scope: Vec::new(),
        static_token: false,
        api_token_id: None,
    });
    assert_eq!(fx.who(&[bearer(&raw)], "s").await, Err(Refusal::Invalid));
    assert_eq!(fx.who(&[basic("dev", &raw)], "s").await, Err(Refusal::Invalid));
    let presented = [Presented {
        transport: Transport::Authorization,
        credential: Credential::Registry(registry),
    }];
    assert_eq!(fx.who(&presented, "s2").await, Err(Refusal::Invalid));
}
