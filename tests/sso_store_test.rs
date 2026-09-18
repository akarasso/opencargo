//! Ports 19, 20 and 21, run against the fake and the SQLite adapter.

mod common;

use std::any::Any;
use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use opencargo::domain::identity::{Authority, IdentityKey, Outage};
use opencargo::error::StoreError;
use opencargo::ports::handoffs::{Consumption, LoginHandoffStore, NewHandoff};
use opencargo::ports::identities::{Admission, DisabledBy, IdentityStore};
use opencargo::ports::secrets::ServerSecretStore;
use opencargo::ports::tokens::{NewToken, TokenStore};
use opencargo::ports::users::{NewUser, UserStore};
use tempfile::TempDir;

pub struct SsoHandles {
    identities: Arc<dyn IdentityStore>,
    handoffs: Arc<dyn LoginHandoffStore>,
    secrets: Arc<dyn ServerSecretStore>,
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    _keep: Box<dyn Any + Send>,
}

async fn fake() -> SsoHandles {
    let db = FakeDb::new();
    SsoHandles {
        identities: db.identities(),
        handoffs: db.handoffs(),
        secrets: db.secrets(),
        users: db.users(),
        tokens: db.tokens(),
        _keep: Box::new(db),
    }
}

async fn sqlite() -> SsoHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("sso.db"))
        .await
        .unwrap();
    SsoHandles {
        identities: stores.identities(),
        handoffs: stores.handoffs(),
        secrets: stores.secrets(),
        users: stores.users(),
        tokens: stores.tokens(),
        _keep: Box::new((tmp, stores)),
    }
}

macro_rules! sso_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;

            #[tokio::test]
            async fn two_concurrent_consumptions_of_one_handoff_have_one_winner() {
                let h = $open().await;
                deposit(&h, "c1", "b1", at(1)).await;
                let calls = (0..8).map(|_| {
                    let store = h.handoffs.clone();
                    tokio::spawn(async move { store.consume("c1", "b1", at(0)).await.unwrap() })
                });
                let mut won = 0;
                for call in calls {
                    match call.await.unwrap() {
                        Consumption::Consumed(p) => {
                            assert_eq!(p, "payload");
                            won += 1;
                        }
                        other => assert_eq!(other, Consumption::AlreadyConsumed),
                    }
                }
                assert_eq!(won, 1);
            }

            #[tokio::test]
            async fn a_mismatched_or_expired_handoff_is_refused_and_burnt() {
                let h = $open().await;
                deposit(&h, "c1", "b1", at(1)).await;
                assert_eq!(
                    h.handoffs.consume("c1", "other", at(0)).await.unwrap(),
                    Consumption::BindingMismatch
                );
                assert_eq!(
                    h.handoffs.consume("c1", "b1", at(0)).await.unwrap(),
                    Consumption::AlreadyConsumed
                );
                deposit(&h, "c2", "b1", at(1)).await;
                assert_eq!(
                    h.handoffs.consume("c2", "b1", at(2)).await.unwrap(),
                    Consumption::Expired
                );
                assert_eq!(
                    h.handoffs.consume("nope", "b1", at(0)).await.unwrap(),
                    Consumption::Unknown
                );
            }

            #[tokio::test]
            async fn purge_removes_only_expired_handoffs_and_peek_consumes_nothing() {
                let h = $open().await;
                deposit(&h, "old", "b", at(1)).await;
                deposit(&h, "live", "b", at(5)).await;
                assert_eq!(
                    h.handoffs
                        .peek("live", "b", at(2))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("payload")
                );
                assert_eq!(h.handoffs.peek("live", "x", at(2)).await.unwrap(), None);
                assert_eq!(h.handoffs.purge_expired(at(2)).await.unwrap(), 1);
                assert!(matches!(
                    h.handoffs.consume("live", "b", at(2)).await.unwrap(),
                    Consumption::Consumed(_)
                ));
                assert_eq!(
                    h.handoffs.consume("old", "b", at(2)).await.unwrap(),
                    Consumption::Unknown
                );
            }

            #[tokio::test]
            async fn a_session_is_issued_only_under_a_live_link_of_an_enabled_account() {
                let h = $open().await;
                let (a, b) = (key("corp", "alice"), key("corp", "bob"));
                let alice = provision(&h, "alice", &a).await;
                let bob = provision(&h, "bob", &b).await;
                refused(&h, bob, "foreign", &a).await;
                refused(&h, alice, "unlinked", &key("corp", "nobody")).await;
                token(&h, alice, "live", Some(&a)).await;
                assert_eq!(
                    h.identities.provenance("live").await.unwrap(),
                    Some(a.clone())
                );
                h.identities
                    .disable_user(alice, DisabledBy::Admin, at(0))
                    .await
                    .unwrap();
                refused(&h, alice, "user-off", &a).await;
                h.identities.enable_user(alice).await.unwrap();
                h.identities.disable_link(&a).await.unwrap();
                refused(&h, alice, "link-off", &a).await;
                h.identities
                    .revoke_authority(&authority("corp"))
                    .await
                    .unwrap();
                refused(&h, bob, "retired", &b).await;
            }

            #[tokio::test]
            async fn concurrent_secret_initialisations_agree() {
                let h = $open().await;
                let calls = (0u8..8).map(|i| {
                    let store = h.secrets.clone();
                    tokio::spawn(async move { store.get_or_init("k", &[i; 32]).await.unwrap() })
                });
                let mut seen = Vec::new();
                for call in calls {
                    seen.push(call.await.unwrap());
                }
                assert!(seen.windows(2).all(|w| w[0] == w[1]));
                assert_eq!(
                    h.secrets.get_or_init("k", &[99; 32]).await.unwrap(),
                    seen[0]
                );
            }

            #[tokio::test]
            async fn revoking_an_authority_revokes_its_tokens_and_disables_its_links() {
                let h = $open().await;
                let alice = provision(&h, "alice", &key("corp", "a")).await;
                let bob = provision(&h, "bob", &key("other", "b")).await;
                let t1 = token(&h, alice, "t1", Some(&key("corp", "a"))).await;
                let t2 = token(&h, alice, "t2", None).await;
                let t3 = token(&h, bob, "t3", Some(&key("other", "b"))).await;
                assert_eq!(
                    h.identities
                        .revoke_authority(&authority("corp"))
                        .await
                        .unwrap(),
                    1
                );
                assert!(h.tokens.by_id(&t1).await.unwrap().is_none());
                assert!(
                    h.tokens.by_id(&t2).await.unwrap().is_some(),
                    "a local token stays"
                );
                assert!(h.tokens.by_id(&t3).await.unwrap().is_some());
                assert!(
                    h.identities
                        .find(&key("corp", "a"))
                        .await
                        .unwrap()
                        .unwrap()
                        .disabled
                );
                assert!(h
                    .identities
                    .login_state(alice)
                    .await
                    .unwrap()
                    .links
                    .is_empty());
                assert_eq!(
                    h.identities.authorities().await.unwrap(),
                    vec![authority("other")]
                );
                assert!(matches!(
                    h.identities
                        .admit(&admission(&key("corp", "a"), "reader"), at(0))
                        .await,
                    Err(StoreError::NotFound)
                ));
            }

            #[tokio::test]
            async fn detaching_restores_the_role_and_revokes_what_the_link_produced() {
                let h = $open().await;
                let local = h
                    .users
                    .create(
                        &NewUser {
                            username: "carol",
                            email: None,
                            password_hash: "x",
                            role: "publisher",
                        },
                        at(0),
                    )
                    .await
                    .unwrap();
                let k = key("corp", "c");
                h.identities
                    .attach(local.id, &k, Some("c@example.com"), at(0))
                    .await
                    .unwrap();
                assert!(matches!(
                    h.identities.attach(local.id, &k, None, at(0)).await,
                    Err(StoreError::Conflict)
                ));
                let t = token(&h, local.id, "tc", Some(&k)).await;
                assert_eq!(h.identities.provenance(&t).await.unwrap(), Some(k.clone()));
                h.identities.detach(local.id, &k).await.unwrap();
                assert!(h.tokens.by_id(&t).await.unwrap().is_none());
                assert_eq!(
                    h.users.by_id(local.id).await.unwrap().unwrap().role,
                    "publisher"
                );
                assert!(h.identities.find(&k).await.unwrap().is_none());
                assert!(matches!(
                    h.identities.detach(local.id, &k).await,
                    Err(StoreError::NotFound)
                ));
            }

            #[tokio::test]
            async fn a_denied_disablement_is_lifted_by_admission_an_admin_one_is_not() {
                let h = $open().await;
                let k = key("corp", "d");
                let dave = provision(&h, "dave", &k).await;
                let t = token(&h, dave, "td", Some(&k)).await;
                h.identities.deprovision(&k, at(0)).await.unwrap();
                assert!(h.identities.login_state(dave).await.unwrap().disabled);
                assert!(h.tokens.by_id(&t).await.unwrap().is_none());
                let user = h
                    .identities
                    .admit(&admission(&k, "publisher"), at(1))
                    .await
                    .unwrap();
                assert_eq!(
                    user.role, "publisher",
                    "a provisioned account follows the rules"
                );
                assert!(!h.identities.login_state(dave).await.unwrap().disabled);
                h.identities
                    .disable_user(dave, DisabledBy::Admin, at(2))
                    .await
                    .unwrap();
                h.identities.deprovision(&k, at(3)).await.unwrap();
                h.identities
                    .admit(&admission(&k, "reader"), at(4))
                    .await
                    .unwrap();
                assert!(h.identities.login_state(dave).await.unwrap().disabled);
                h.identities.enable_user(dave).await.unwrap();
                assert!(!h.identities.login_state(dave).await.unwrap().disabled);
            }

            #[tokio::test]
            async fn admission_applies_managed_grants_only() {
                let h = $open().await;
                let k = key("corp", "e");
                provision(&h, "erin", &k).await;
                let found = h.identities.find(&k).await.unwrap().unwrap();
                assert!(found.provisioned);
                let clash = h
                    .identities
                    .provision(
                        &NewUser {
                            username: "erin",
                            email: None,
                            password_hash: "!",
                            role: "reader",
                        },
                        &admission(&key("corp", "other"), "reader"),
                        at(0),
                    )
                    .await;
                assert!(matches!(clash, Err(StoreError::Conflict)));
                assert!(h
                    .identities
                    .find(&key("corp", "other"))
                    .await
                    .unwrap()
                    .is_none());
            }

            #[tokio::test]
            async fn probes_open_and_close_outages_seen_by_login_state() {
                let h = $open().await;
                let k = key("corp", "f");
                let frank = provision(&h, "frank", &k).await;
                let a = authority("corp");
                h.identities.record_probe(&a, true, at(1)).await.unwrap();
                h.identities.record_probe(&a, false, at(2)).await.unwrap();
                h.identities.record_probe(&a, false, at(3)).await.unwrap();
                h.identities.record_probe(&a, true, at(4)).await.unwrap();
                h.identities.record_probe(&a, false, at(5)).await.unwrap();
                assert_eq!(
                    h.identities.outages(&a).await.unwrap(),
                    vec![
                        Outage {
                            start: at(2),
                            end: Some(at(4))
                        },
                        Outage {
                            start: at(5),
                            end: None
                        }
                    ]
                );
                let state = h.identities.login_state(frank).await.unwrap();
                assert_eq!(state.links.len(), 1);
                assert_eq!(state.links[0].outages.len(), 2);
            }

            #[tokio::test]
            async fn a_migrated_authority_keeps_its_links_and_provenance() {
                let h = $open().await;
                let k = key("corp", "g");
                let gina = provision(&h, "gina", &k).await;
                let t = token(&h, gina, "tg", Some(&k)).await;
                let to = Authority::new("corp", "https://new.example");
                h.identities
                    .migrate_authority(&authority("corp"), &to)
                    .await
                    .unwrap();
                let moved = IdentityKey {
                    authority: to.clone(),
                    subject: "g".into(),
                };
                assert!(h.identities.find(&moved).await.unwrap().is_some());
                assert_eq!(h.identities.provenance(&t).await.unwrap(), Some(moved));
                assert_eq!(h.identities.revoke_authority(&to).await.unwrap(), 1);
            }
        }
    };
}

fn at(hour: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 18, 0, 0, 0).unwrap() + Duration::hours(hour)
}

fn authority(provider: &str) -> Authority {
    Authority::new(provider, &format!("https://{provider}.example"))
}

fn key(provider: &str, subject: &str) -> IdentityKey {
    IdentityKey {
        authority: authority(provider),
        subject: subject.to_string(),
    }
}

fn admission<'a>(key: &'a IdentityKey, role: &'a str) -> Admission<'a> {
    Admission {
        key,
        email: None,
        role,
        grants: &[],
        managed: &[],
    }
}

async fn deposit(h: &SsoHandles, code: &str, binding: &str, expires: DateTime<Utc>) {
    h.handoffs
        .deposit(&NewHandoff {
            code_hash: code,
            binding,
            payload: "payload",
            expires_at: expires,
        })
        .await
        .unwrap();
}

async fn provision(h: &SsoHandles, name: &str, key: &IdentityKey) -> i64 {
    h.identities
        .provision(
            &NewUser {
                username: name,
                email: None,
                password_hash: "!sso",
                role: "reader",
            },
            &admission(key, "reader"),
            at(0),
        )
        .await
        .unwrap()
        .id
}

fn new_token<'a>(user: i64, id: &'a str, prefix: &'a str) -> NewToken<'a> {
    NewToken {
        id,
        user_id: user,
        name: id,
        prefix,
        token_hash: "h",
        expires_at: None,
    }
}

async fn token(h: &SsoHandles, user: i64, id: &str, from: Option<&IdentityKey>) -> String {
    let prefix = format!("{id:0>16}");
    let token = new_token(user, id, &prefix);
    match from {
        Some(key) => h
            .identities
            .issue_session(&token, key, at(0))
            .await
            .unwrap(),
        None => {
            h.tokens.create(&token, at(0)).await.unwrap();
        }
    }
    id.to_string()
}

async fn refused(h: &SsoHandles, user: i64, id: &str, key: &IdentityKey) {
    let prefix = format!("{id:0>16}");
    let got = h
        .identities
        .issue_session(&new_token(user, id, &prefix), key, at(0))
        .await;
    assert!(matches!(got, Err(StoreError::NotFound)), "{id}: {got:?}");
    assert!(h.tokens.by_id(id).await.unwrap().is_none(), "{id}");
}

sso_contract!(fake_db, fake);
sso_contract!(sqlite_adapter, sqlite);
