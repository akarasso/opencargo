use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::app::authenticate::AuthenticateDeps;
use crate::domain::identity::{
    Declared, GrantRole, GroupGrant, Groups, LoginPolicy, ProviderProfile,
};
use crate::domain::RepoSpec;
use crate::registry::oci::token::TokenSigner;
use crate::testing::fakes::{FakeDb, PortId};

const ISSUER: &str = "https://idp.example";

struct Now;

impl Clock for Now {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

struct Seq(Mutex<u64>);

impl Ids for Seq {
    fn token_id(&self) -> String {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        format!("tok-{n}")
    }

    fn upload_id(&self) -> String {
        self.token_id()
    }
}

/// An `IdentityProvider` with no HTTP: `finish` answers what the test set.
struct FakeProvider {
    profile: ProviderProfile,
    answer: Mutex<Result<ExternalIdentity, IdpFailure>>,
    reachable: Mutex<bool>,
    finished: Mutex<usize>,
}

#[async_trait]
impl IdentityProvider for FakeProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn start(&self, _redirect_uri: &str) -> Result<(String, Attempt), IdpFailure> {
        let attempt = Attempt {
            provider: self.profile.authority.provider.clone(),
            state: random_code(),
            nonce: random_code(),
            verifier: random_code(),
        };
        Ok((
            format!("{ISSUER}/authorize?state={}", attempt.state),
            attempt,
        ))
    }

    async fn finish(
        &self,
        _attempt: &Attempt,
        _callback: &Callback,
        _redirect_uri: &str,
    ) -> Result<ExternalIdentity, IdpFailure> {
        *self.finished.lock().unwrap() += 1;
        self.answer.lock().unwrap().clone()
    }

    async fn probe(&self) -> bool {
        *self.reachable.lock().unwrap()
    }

    async fn end_session(&self, _post_logout_redirect: &str) -> Option<String> {
        Some(format!("{ISSUER}/logout"))
    }
}

struct Fx {
    store: FakeDb,
    idp: Arc<FakeProvider>,
    sso: Sso,
}

fn identity(subject: &str, groups: &[&str]) -> ExternalIdentity {
    ExternalIdentity {
        key: IdentityKey {
            authority: Authority::new("corp", ISSUER),
            subject: subject.into(),
        },
        email: Some(format!("{subject}@example.com")),
        email_trust: EmailTrust::Verified,
        tenant: None,
        groups: Groups::Listed(groups.iter().map(|g| g.to_string()).collect()),
        preferred_name: Some(subject.into()),
    }
}

fn fixture() -> Fx {
    let db = FakeDb::new();
    let idp = Arc::new(FakeProvider {
        profile: ProviderProfile {
            authority: Authority::new("corp", ISSUER),
            open: false,
            allow_open: false,
            tenant_pinned: false,
            authoritative_domains: vec!["example.com".into()],
            policy: LoginPolicy {
                required_groups: vec!["dev".into()],
                allowed_domains: vec![],
                default_role: "reader".into(),
                grants: vec![GroupGrant {
                    group: "dev".into(),
                    repository: "private".into(),
                    role: GrantRole::Publisher,
                }],
            },
        },
        answer: Mutex::new(Ok(identity("alice", &["dev"]))),
        reachable: Mutex::new(true),
        finished: Mutex::new(0),
    });
    let authenticate = Arc::new(Authenticate::new(AuthenticateDeps {
        static_tokens: vec![],
        token_prefix: "trg_".into(),
        users: db.users(),
        tokens: db.tokens(),
        signer: Arc::new(TokenSigner::random()),
        login_limiter: Arc::new(RateLimiter::new(5, 60)),
        token_limiter: Arc::new(RateLimiter::new(5, 60)),
        gate: Arc::new(crate::app::login_gate::PolicyGate::new(
            db.identities(),
            Default::default(),
            Some("root".into()),
        )),
        clock: Arc::new(Now),
    }));
    let sso = Sso::new(SsoDeps {
        providers: vec![idp.clone()],
        identities: db.identities(),
        handoffs: db.handoffs(),
        users: db.users(),
        tokens: db.tokens(),
        repos: db.repositories(),
        audit: db.audit(),
        ids: Arc::new(Seq(Mutex::new(0))),
        clock: Arc::new(Now),
        authenticate,
        sealer: Arc::new(Sealer::new(&Sealer::random_key()).unwrap()),
        settings: SsoSettings {
            base_url: "http://registry.test".into(),
            session_ttl: Duration::hours(12),
            handoff_ttl: Duration::minutes(2),
            attempt_ttl: Duration::minutes(10),
            bootstrap: Some("root".into()),
        },
    });
    Fx { store: db, idp, sso }
}

fn callback_for(begun: &Begun) -> Callback {
    let state = begun.location.split("state=").nth(1).unwrap().to_string();
    Callback {
        code: Some("idp-code".into()),
        state: Some(state),
        iss: None,
        error: None,
    }
}

fn code_of(landed: &Landed) -> String {
    landed.location.split("code=").nth(1).unwrap().to_string()
}

impl Fx {
    async fn land(&self) -> SsoResult<(Landed, String)> {
        let begun = self.sso.begin_login("corp", "/packages").await?;
        let landed = self
            .sso
            .complete(
                "corp",
                &callback_for(&begun),
                Some(&begun.cookie),
                Meta::default(),
            )
            .await?;
        Ok((landed, begun.cookie))
    }

    async fn login(&self) -> SsoResult<Session> {
        let (landed, cookie) = self.land().await?;
        self.sso
            .exchange(&code_of(&landed), Some(&cookie), Meta::default())
            .await
    }

    fn answer(&self, identity: Result<ExternalIdentity, IdpFailure>) {
        *self.idp.answer.lock().unwrap() = identity;
    }

    async fn local(&self, name: &str, role: &str) -> User {
        let hash = crate::auth::users::hash_password("pw-correct").unwrap();
        self.store
            .users()
            .create(
                &NewUser {
                    username: name,
                    email: None,
                    password_hash: &hash,
                    role,
                },
                Utc::now(),
            )
            .await
            .unwrap()
    }

    async fn private_repo(&self) {
        self.store
            .repositories()
            .create(
                &RepoSpec {
                    name: "private",
                    kind: crate::domain::RepoKind::Hosted,
                    format: crate::domain::Format::Npm,
                    visibility: Visibility::Private,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_login_provisions_grants_and_hands_off_a_token_with_provenance() {
    let fx = fixture();
    fx.private_repo().await;
    let session = fx.login().await.unwrap();
    assert_eq!(session.username, "alice");
    assert_eq!(session.return_to, "/packages");
    let user = fx.store.users().by_name("alice").await.unwrap().unwrap();
    let tokens = fx.store.tokens().of_user(user.id).await.unwrap();
    assert_eq!(tokens.len(), 1);
    assert!(tokens[0].expires_at.is_some(), "bounded by session_ttl");
    assert!(fx
        .store
        .identities()
        .provenance(&tokens[0].id)
        .await
        .unwrap()
        .is_some());
    let rights = fx.store.perms().rights(user.id, 1).await.unwrap().unwrap();
    assert!(rights.write);
}

#[tokio::test]
async fn nothing_is_written_before_the_cookie_and_state_match() {
    let fx = fixture();
    let begun = fx.sso.begin_login("corp", "/").await.unwrap();
    let mut forged = callback_for(&begun);
    forged.state = Some("other".into());
    for (callback, cookie) in [
        (forged.clone(), Some(begun.cookie.as_str())),
        (callback_for(&begun), None),
        (callback_for(&begun), Some("garbage")),
    ] {
        assert!(matches!(
            fx.sso
                .complete("corp", &callback, cookie, Meta::default())
                .await,
            Err(SsoError::Refused(SsoRefusal::StateMismatch))
        ));
    }
    assert_eq!(
        *fx.idp.finished.lock().unwrap(),
        0,
        "the IdP is never called"
    );
    assert!(fx.store.users().all().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejected_changes_nothing_and_only_denied_deprovisions() {
    let fx = fixture();
    let session = fx.login().await.unwrap();
    let alice = fx.store.users().by_name("alice").await.unwrap().unwrap();

    let mut unreadable = identity("alice", &[]);
    unreadable.groups = Groups::Unreadable;
    fx.answer(Ok(unreadable));
    assert!(matches!(
        fx.login().await,
        Err(SsoError::Refused(SsoRefusal::Rejected))
    ));
    assert!(
        !fx.store
            .identities()
            .login_state(alice.id)
            .await
            .unwrap()
            .disabled
    );
    assert!(fx
        .store
        .tokens()
        .by_prefix(&session.token[..16])
        .await
        .unwrap()
        .is_some());

    fx.answer(Ok(identity("alice", &["ops"])));
    assert!(matches!(
        fx.login().await,
        Err(SsoError::Refused(SsoRefusal::Denied))
    ));
    assert!(
        fx.store
            .identities()
            .login_state(alice.id)
            .await
            .unwrap()
            .disabled
    );
    assert!(
        fx.store
            .tokens()
            .by_prefix(&session.token[..16])
            .await
            .unwrap()
            .is_none(),
        "the derived session is revoked with the deprovisioning"
    );

    fx.answer(Ok(identity("bob", &["ops"])));
    assert!(matches!(
        fx.login().await,
        Err(SsoError::Refused(SsoRefusal::Denied))
    ));
    assert!(fx.store.users().by_name("bob").await.unwrap().is_none());
}

#[tokio::test]
async fn a_handoff_is_exchanged_once_and_only_with_its_cookie() {
    let fx = fixture();
    let (landed, cookie) = fx.land().await.unwrap();
    let code = code_of(&landed);
    assert!(matches!(
        fx.sso.exchange(&code, None, Meta::default()).await,
        Err(SsoError::Refused(SsoRefusal::BindingMismatch))
    ));
    assert!(
        matches!(
            fx.sso.exchange(&code, Some(&cookie), Meta::default()).await,
            Err(SsoError::Refused(SsoRefusal::HandoffConsumed)),
        ),
        "a refused exchange consumed it"
    );

    let (landed, cookie) = fx.land().await.unwrap();
    let code = code_of(&landed);
    fx.sso
        .exchange(&code, Some(&cookie), Meta::default())
        .await
        .unwrap();
    assert!(matches!(
        fx.sso.exchange(&code, Some(&cookie), Meta::default()).await,
        Err(SsoError::Refused(SsoRefusal::HandoffConsumed))
    ));
}

#[tokio::test]
async fn a_name_taken_by_an_unlinked_local_account_is_refused() {
    let fx = fixture();
    fx.local("alice", "publisher").await;
    assert!(matches!(
        fx.login().await,
        Err(SsoError::Refused(SsoRefusal::NameCollision))
    ));
    let alice = fx.store.users().by_name("alice").await.unwrap().unwrap();
    assert!(fx
        .store
        .identities()
        .of_user(alice.id)
        .await
        .unwrap()
        .is_empty());
}

async fn begin_link(fx: &Fx, user: &User) -> (String, String) {
    let begun = fx
        .sso
        .begin_link(user.id, "pw-correct", "corp")
        .await
        .unwrap();
    let landed = fx
        .sso
        .complete(
            "corp",
            &callback_for(&begun),
            Some(&begun.cookie),
            Meta::default(),
        )
        .await
        .unwrap();
    assert!(landed.location.starts_with("/login/sso/link#code="));
    (code_of(&landed), begun.cookie)
}

#[tokio::test]
async fn a_link_needs_the_confirmation_of_the_named_account_with_its_cookie() {
    let fx = fixture();
    let carol = fx.local("carol", "publisher").await;
    let mallory = fx.local("mallory", "reader").await;
    fx.answer(Ok(identity("carol-sso", &["dev"])));

    let (code, cookie) = begin_link(&fx, &carol).await;
    let shown = fx.sso.link_details(&code, Some(&cookie)).await.unwrap();
    assert_eq!(shown.target, "carol");
    assert_eq!(shown.issuer, ISSUER);
    assert!(shown.proposal);
    assert!(
        fx.store
            .identities()
            .of_user(carol.id)
            .await
            .unwrap()
            .is_empty(),
        "nothing before the human"
    );
    assert!(matches!(
        fx.sso
            .confirm_link(mallory.id, &code, Some(&cookie), Meta::default())
            .await,
        Err(SsoError::Refused(SsoRefusal::WrongUser))
    ));
    assert!(
        matches!(
            fx.sso
                .confirm_link(carol.id, &code, Some(&cookie), Meta::default())
                .await,
            Err(SsoError::Refused(SsoRefusal::HandoffConsumed))
        ),
        "the wrong Bearer burnt the handoff"
    );

    let (code, _cookie) = begin_link(&fx, &carol).await;
    assert!(matches!(
        fx.sso
            .confirm_link(carol.id, &code, None, Meta::default())
            .await,
        Err(SsoError::Refused(SsoRefusal::BindingMismatch))
    ));

    let (code, cookie) = begin_link(&fx, &carol).await;
    fx.sso
        .confirm_link(carol.id, &code, Some(&cookie), Meta::default())
        .await
        .unwrap();
    let links = fx.store.identities().of_user(carol.id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert!(!links[0].provisioned);
    assert_eq!(
        fx.store.users().by_id(carol.id).await.unwrap().unwrap().role,
        "publisher"
    );
}

#[tokio::test]
async fn linking_an_admin_account_is_refused() {
    let fx = fixture();
    let admin = fx.local("boss", "admin").await;
    assert!(matches!(
        fx.sso.begin_link(admin.id, "pw-correct", "corp").await,
        Err(SsoError::Refused(SsoRefusal::AdminNotLinkable))
    ));
    let root = fx.local("root", "publisher").await;
    assert!(
        matches!(
            fx.sso.begin_link(root.id, "pw-correct", "corp").await,
            Err(SsoError::Refused(SsoRefusal::AdminNotLinkable))
        ),
        "the bootstrap account is never linked"
    );
    let dev = fx.local("dev", "reader").await;
    assert!(matches!(
        fx.sso.begin_link(dev.id, "wrong", "corp").await,
        Err(SsoError::Credentials(_))
    ));
}

#[tokio::test]
async fn disabling_an_account_revokes_its_sessions_at_once() {
    let fx = fixture();
    let session = fx.login().await.unwrap();
    let alice = fx.store.users().by_name("alice").await.unwrap().unwrap();
    fx.sso.disable_user(&alice).await.unwrap();
    assert!(fx
        .store
        .tokens()
        .by_prefix(&session.token[..16])
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        fx.login().await,
        Err(SsoError::Refused(SsoRefusal::Disabled))
    ));
    let root = fx.local("root", "admin").await;
    assert!(
        fx.sso.disable_user(&root).await.is_err(),
        "the bootstrap account is never locked"
    );
}

#[tokio::test]
async fn a_store_fault_is_an_error_not_a_refusal() {
    let fx = fixture();
    let (landed, cookie) = fx.land().await.unwrap();
    fx.store.fail_next(PortId::Handoffs, StoreError::Unavailable);
    assert!(matches!(
        fx.sso
            .exchange(&code_of(&landed), Some(&cookie), Meta::default())
            .await,
        Err(SsoError::Store(StoreError::Unavailable))
    ));
}

#[tokio::test]
async fn an_absent_authority_without_a_declaration_refuses_the_start_and_revokes_nothing() {
    let fx = fixture();
    let session = fx.login().await.unwrap();
    let corp = Authority::new("corp", ISSUER);
    let refused = reconcile_providers(
        fx.store.identities().as_ref(),
        &Declared {
            active: &[],
            retired: &[],
            migrated: &[],
        },
    )
    .await;
    assert!(refused.unwrap_err().to_string().contains("corp"));
    assert!(fx
        .store
        .tokens()
        .by_prefix(&session.token[..16])
        .await
        .unwrap()
        .is_some());

    reconcile_providers(
        fx.store.identities().as_ref(),
        &Declared {
            active: &[],
            retired: std::slice::from_ref(&corp),
            migrated: &[],
        },
    )
    .await
    .unwrap();
    assert!(fx
        .store
        .tokens()
        .by_prefix(&session.token[..16])
        .await
        .unwrap()
        .is_none());
    assert!(
        reconcile_providers(
            fx.store.identities().as_ref(),
            &Declared {
                active: &[],
                retired: &[],
                migrated: &[]
            }
        )
        .await
        .is_ok(),
        "a retired authority is no longer a known one"
    );
}

#[tokio::test]
async fn forged_callbacks_never_feed_the_reauthentication_clock() {
    let fx = fixture();
    fx.login().await.unwrap();
    fx.answer(Err(IdpFailure::IdpError("429".into())));
    for _ in 0..50 {
        let _ = fx.land().await;
    }
    fx.sso.probe_all().await;
    let corp = Authority::new("corp", ISSUER);
    assert!(fx.store.identities().outages(&corp).await.unwrap().is_empty());
    *fx.idp.reachable.lock().unwrap() = false;
    fx.sso.probe_all().await;
    assert_eq!(fx.store.identities().outages(&corp).await.unwrap().len(), 1);
}

#[tokio::test]
async fn logout_revokes_the_sso_session_and_names_the_end_session_url() {
    let fx = fixture();
    let session = fx.login().await.unwrap();
    let stored = fx
        .store
        .tokens()
        .by_prefix(&session.token[..16])
        .await
        .unwrap()
        .unwrap();
    let end = fx.sso.logout(Some(&stored.id)).await.unwrap();
    assert_eq!(end.as_deref(), Some("https://idp.example/logout"));
    assert!(fx.store.tokens().by_id(&stored.id).await.unwrap().is_none());
}
