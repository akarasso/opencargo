//! What `TokenStore` owes the layers above about the scope it carries, run
//! against the fake the unit tests use and the SQLite adapter the server runs
//! on. A scope that survived one and not the other would be a claim about an
//! adapter, not about the port.

mod common;

use std::any::Any;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use opencargo::domain::scope::{Grant, Pattern, ScopeAction, Selector};
use opencargo::domain::TokenScope;
use opencargo::ports::tokens::{NewToken, TokenStore};
use opencargo::ports::users::{NewUser, UserStore};
use tempfile::TempDir;

pub struct TokenHandles {
    tokens: Arc<dyn TokenStore>,
    users: Arc<dyn UserStore>,
    _keep: Box<dyn Any + Send>,
}

async fn fake() -> TokenHandles {
    let db = FakeDb::new();
    TokenHandles {
        tokens: db.tokens(),
        users: db.users(),
        _keep: Box::new(db),
    }
}

async fn sqlite() -> TokenHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("tokens.db"))
        .await
        .unwrap();
    TokenHandles {
        tokens: stores.tokens(),
        users: stores.users(),
        _keep: Box::new((tmp, stores)),
    }
}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 19, hour, 0, 0).unwrap()
}

fn limited() -> TokenScope {
    TokenScope::Limited {
        grants: vec![
            Grant {
                selector: Selector::Repo {
                    repo: Pattern::parse("libs-*").unwrap(),
                },
                actions: vec![ScopeAction::Read, ScopeAction::Write],
                incarnations: vec!["inc-1".to_string(), "inc-2".to_string()],
            },
            Grant {
                selector: Selector::Package {
                    repo: Pattern::parse("npm").unwrap(),
                    package: Pattern::parse("@acme/*").unwrap(),
                },
                actions: vec![ScopeAction::Delete],
                incarnations: vec!["inc-3".to_string()],
            },
        ],
    }
}

async fn account(h: &TokenHandles, name: &str) -> i64 {
    h.users
        .create(
            &NewUser {
                username: name,
                email: None,
                password_hash: "h",
                role: "publisher",
            },
            at(9),
        )
        .await
        .unwrap()
        .id
}

macro_rules! token_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;

            #[tokio::test]
            async fn a_scope_round_trips_through_every_lookup() {
                let h = $open().await;
                let user = account(&h, "ci").await;
                let scope = limited();

                let created = h
                    .tokens
                    .create(
                        &NewToken {
                            id: "t1",
                            user_id: user,
                            name: "robot",
                            prefix: "trgs_0000000000",
                            token_hash: "hash",
                            expires_at: None,
                            scope: &scope,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                assert_eq!(created.scope, scope);
                assert_eq!(h.tokens.by_id("t1").await.unwrap().unwrap().scope, scope);
                assert_eq!(
                    h.tokens
                        .by_prefix("trgs_0000000000")
                        .await
                        .unwrap()
                        .unwrap()
                        .scope,
                    scope
                );
                assert_eq!(h.tokens.of_user(user).await.unwrap()[0].scope, scope);
            }

            #[tokio::test]
            async fn an_unscoped_token_stays_inherit() {
                let h = $open().await;
                let user = account(&h, "alice").await;

                h.tokens
                    .create(
                        &NewToken {
                            id: "t2",
                            user_id: user,
                            name: "session",
                            prefix: "trg_00000000000",
                            token_hash: "hash",
                            expires_at: None,
                            scope: &TokenScope::Inherit,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                let stored = h.tokens.by_id("t2").await.unwrap().unwrap();
                assert!(stored.scope.is_inherit());
            }

            /// Revoking is the only way a scope changes, so what the port owes
            /// is that the credential is gone at the next lookup.
            #[tokio::test]
            async fn revoking_takes_the_scope_with_it() {
                let h = $open().await;
                let user = account(&h, "bob").await;
                let scope = limited();
                h.tokens
                    .create(
                        &NewToken {
                            id: "t3",
                            user_id: user,
                            name: "robot",
                            prefix: "trgs_1111111111",
                            token_hash: "hash",
                            expires_at: None,
                            scope: &scope,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                h.tokens.delete("t3").await.unwrap();

                assert!(h.tokens.by_id("t3").await.unwrap().is_none());
                assert!(h.tokens.by_prefix("trgs_1111111111").await.unwrap().is_none());
            }
        }
    };
}

token_contract!(fake_db, fake);
token_contract!(sqlite_adapter, sqlite);
