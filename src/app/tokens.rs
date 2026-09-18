//! Issuing and revoking the API tokens that stand for an account.
//!
//! The raw token exists for the length of one response: it is generated here,
//! hashed before the store sees it, and returned to the caller once. A
//! revocation checks that the token is the account's own, because the route
//! only knows which account was named, not which tokens it holds.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::app::audit::{self, Actor};
use crate::app::users::load_account;
use crate::auth::tokens as credentials;
use crate::domain::ApiToken;
use crate::error::{AppError, AppResult};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::ids::Ids;
use crate::ports::tokens::{NewToken, TokenStore};
use crate::ports::users::UserStore;

/// The prefix every issued token carries, and the length of the lookup key
/// cut from its front.
const TOKEN_PREFIX: &str = "trg_";
const KEY_LEN: usize = 16;

/// A token as its owner sees it exactly once.
pub struct Issued {
    pub id: String,
    pub name: String,
    pub prefix: String,
    /// The only copy: nothing stores it.
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct IssueToken {
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    ids: Arc<dyn Ids>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl IssueToken {
    pub fn new(
        users: Arc<dyn UserStore>,
        tokens: Arc<dyn TokenStore>,
        ids: Arc<dyn Ids>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            users,
            tokens,
            ids,
            audit,
            events,
        }
    }

    pub async fn run(
        &self,
        username: &str,
        name: &str,
        expires_in_days: Option<i64>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<Issued> {
        let user = load_account(&*self.users, username).await?;

        let id = self.ids.token_id();
        let (token, token_hash) = credentials::generate_token(TOKEN_PREFIX);
        let prefix = token[..KEY_LEN].to_string();
        let expires_at = expires_in_days.map(|days| now + Duration::days(days));

        self.tokens
            .create(
                &NewToken {
                    id: &id,
                    user_id: user.id,
                    name,
                    prefix: &prefix,
                    token_hash: &token_hash,
                    expires_at,
                },
                now,
            )
            .await?;

        audit::record(
            &*self.audit,
            &*self.events,
            by,
            "token.create",
            Some(username),
            now,
        )
        .await;
        Ok(Issued {
            id,
            name: name.to_string(),
            prefix,
            token,
            expires_at,
        })
    }
}

pub struct RevokeToken {
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl RevokeToken {
    pub fn new(
        users: Arc<dyn UserStore>,
        tokens: Arc<dyn TokenStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            users,
            tokens,
            audit,
            events,
        }
    }

    /// The route authorized the caller against the *account*, so the token
    /// has to be shown to belong to it before it is removed — otherwise
    /// knowing an id would be enough to revoke anyone's.
    pub async fn run(
        &self,
        username: &str,
        token_id: &str,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let user = load_account(&*self.users, username).await?;
        let token: ApiToken = self
            .tokens
            .by_id(token_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("token not found: {token_id}")))?;
        if token.user_id != user.id {
            return Err(AppError::Forbidden(
                "token does not belong to this user".to_string(),
            ));
        }

        self.tokens.delete(token_id).await?;
        audit::record(
            &*self.audit,
            &*self.events,
            by,
            "token.revoke",
            Some(username),
            now,
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::users::{CreateUser, NewAccount};
    use crate::testing::fakes::FakeDb;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Identifiers a test can predict, and the seam the port exists for.
    #[derive(Default)]
    struct SeqIds(AtomicU32);

    impl Ids for SeqIds {
        fn token_id(&self) -> String {
            format!("tok-{}", self.0.fetch_add(1, Ordering::Relaxed))
        }

        fn upload_id(&self) -> String {
            format!("up-{}", self.0.fetch_add(1, Ordering::Relaxed))
        }
    }

    fn by() -> Actor<'static> {
        Actor {
            user_id: Some(1),
            username: "root",
            admin: true,
        }
    }

    async fn account(db: &FakeDb, username: &str) {
        CreateUser::new(db.users(), db.audit(), crate::server::event_bus())
            .run(
                &NewAccount {
                    username,
                    email: None,
                    role: Some("reader"),
                },
                &by(),
                Utc::now(),
            )
            .await
            .unwrap();
    }

    async fn issue(db: &FakeDb, username: &str, days: Option<i64>) -> Issued {
        IssueToken::new(
            db.users(),
            db.tokens(),
            Arc::new(SeqIds::default()),
            db.audit(),
            crate::server::event_bus(),
        )
        .run(username, "ci", days, &by(), Utc::now())
        .await
        .unwrap()
    }

    /// The raw value is returned once and never stored; what is stored is the
    /// hash and the prefix a lookup is keyed on.
    #[tokio::test]
    async fn an_issued_token_is_returned_once_and_stored_hashed() {
        let db = FakeDb::new();
        account(&db, "alice").await;

        let issued = issue(&db, "alice", None).await;

        assert!(issued.token.starts_with(TOKEN_PREFIX));
        assert_eq!(issued.prefix, issued.token[..KEY_LEN]);
        let stored = db.tokens().by_prefix(&issued.prefix).await.unwrap().unwrap();
        assert!(credentials::verify_token(&issued.token, &stored.token_hash));
        assert_eq!(stored.expires_at, None);
    }

    /// Knowing an id is not enough: a token is revocable only through the
    /// account that holds it.
    #[tokio::test]
    async fn a_token_of_another_account_is_not_revocable() {
        let db = FakeDb::new();
        account(&db, "alice").await;
        account(&db, "bob").await;
        let issued = issue(&db, "alice", None).await;

        let refused = RevokeToken::new(
            db.users(),
            db.tokens(),
            db.audit(),
            crate::server::event_bus(),
        )
        .run("bob", &issued.id, &by(), Utc::now())
        .await
        .unwrap_err();

        assert!(matches!(refused, AppError::Forbidden(_)), "{refused:?}");
        assert!(db.tokens().by_id(&issued.id).await.unwrap().is_some());
    }

    /// The expiry is the caller's clock plus their window, never the store's.
    #[tokio::test]
    async fn an_expiry_is_counted_from_the_clock_the_caller_passed() {
        let db = FakeDb::new();
        account(&db, "alice").await;

        let issued = issue(&db, "alice", Some(7)).await;

        let stored = db.tokens().by_id(&issued.id).await.unwrap().unwrap();
        assert_eq!(stored.expires_at, issued.expires_at);
        assert!(issued.expires_at.unwrap() > Utc::now() + Duration::days(6));
    }
}
