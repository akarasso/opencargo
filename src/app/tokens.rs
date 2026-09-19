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
use crate::domain::{ApiToken, Incarnation, TokenScope};
use crate::error::{AppError, AppResult};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::ids::Ids;
use crate::ports::repositories::RepositoryStore;
use crate::ports::tokens::{NewToken, TokenStore};
use crate::ports::users::UserStore;

/// The length of the lookup key cut from the front of an issued token; the
/// form it carries is the composition root's, not this module's.
const KEY_LEN: usize = 16;

/// A token as its owner sees it exactly once.
pub struct Issued {
    pub id: String,
    pub name: String,
    pub prefix: String,
    /// The only copy: nothing stores it.
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub scope: TokenScope,
}

pub struct IssueToken {
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    repos: Arc<dyn RepositoryStore>,
    ids: Arc<dyn Ids>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
    /// The form the composition root issues under; a scoped credential takes
    /// its sibling form, which an older binary does not recognise.
    prefix: String,
}

pub struct IssueTokenDeps {
    pub users: Arc<dyn UserStore>,
    pub tokens: Arc<dyn TokenStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub ids: Arc<dyn Ids>,
    pub audit: Arc<dyn AuditStore>,
    pub events: Arc<dyn Events>,
    pub prefix: String,
}

impl IssueToken {
    pub fn new(deps: IssueTokenDeps) -> Self {
        Self {
            users: deps.users,
            tokens: deps.tokens,
            repos: deps.repos,
            ids: deps.ids,
            audit: deps.audit,
            events: deps.events,
            prefix: deps.prefix,
        }
    }

    pub async fn run(
        &self,
        username: &str,
        name: &str,
        expires_in_days: Option<i64>,
        scope: &TokenScope,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<Issued> {
        if by.scoped {
            return Err(AppError::Forbidden(
                "a scoped credential never issues another credential".to_string(),
            ));
        }
        scope
            .validate()
            .map_err(|e| AppError::BadRequest(format!("invalid scope: {e}")))?;
        let user = load_account(&*self.users, username).await?;
        let scope = self.resolve(scope).await?;

        let id = self.ids.token_id();
        let (token, token_hash) = credentials::generate_token(&self.prefix, !scope.is_inherit());
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
                    scope: &scope,
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
            scope,
        })
    }

    /// Patterns become incarnations here, once: what the scope holds is the
    /// repositories that existed when it was written, never the name.
    async fn resolve(&self, scope: &TokenScope) -> AppResult<TokenScope> {
        if scope.is_inherit() {
            return Ok(TokenScope::Inherit);
        }
        // One lookup per repository, on the coldest path there is: a token is
        // issued once and judged on every request afterwards.
        let mut world = Vec::new();
        for repo in self.repos.all().await? {
            if let Some(incarnation) = self.repos.incarnation(repo.id).await? {
                world.push((repo.name, incarnation));
            }
        }
        let world: Vec<Incarnation<'_>> = world
            .iter()
            .map(|(name, incarnation)| Incarnation { name, incarnation })
            .collect();
        Ok(scope.resolved(&world))
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
            scoped: false,
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
        issue_scoped(db, username, days, &TokenScope::Inherit, &by()).await.unwrap()
    }

    fn issuer(db: &FakeDb) -> IssueToken {
        IssueToken::new(IssueTokenDeps {
            users: db.users(),
            tokens: db.tokens(),
            repos: db.repositories(),
            ids: Arc::new(SeqIds::default()),
            audit: db.audit(),
            events: crate::server::event_bus(),
            prefix: "trg_".to_string(),
        })
    }

    async fn issue_scoped(
        db: &FakeDb,
        username: &str,
        days: Option<i64>,
        scope: &TokenScope,
        by: &Actor<'_>,
    ) -> AppResult<Issued> {
        issuer(db)
            .run(username, "ci", days, scope, by, Utc::now())
            .await
    }

    fn refusal(got: AppResult<Issued>) -> AppError {
        match got {
            Err(e) => e,
            Ok(_) => panic!("a token was issued"),
        }
    }

    async fn repository(db: &FakeDb, name: &str) -> String {
        let repo = db
            .repositories()
            .create(
                &crate::domain::RepoSpec {
                    name,
                    kind: crate::domain::RepoKind::Hosted,
                    format: crate::domain::Format::Npm,
                    visibility: crate::domain::Visibility::Private,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
        db.repositories()
            .incarnation(repo.id)
            .await
            .unwrap()
            .unwrap()
    }

    fn repo_scope(pattern: &str, actions: &[crate::domain::ScopeAction]) -> TokenScope {
        TokenScope::Limited {
            grants: vec![crate::domain::Grant {
                selector: crate::domain::Selector::Repo {
                    repo: crate::domain::Pattern::parse(pattern).unwrap(),
                },
                actions: actions.to_vec(),
                incarnations: Vec::new(),
            }],
        }
    }

    /// The raw value is returned once and never stored; what is stored is the
    /// hash and the prefix a lookup is keyed on.
    #[tokio::test]
    async fn an_issued_token_is_returned_once_and_stored_hashed() {
        let db = FakeDb::new();
        account(&db, "alice").await;

        let issued = issue(&db, "alice", None).await;

        assert!(issued.token.starts_with("trg_"));
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

    /// The scoped credential takes a form of its own and freezes the
    /// incarnations its pattern named, not the pattern.
    #[tokio::test]
    async fn a_scoped_token_carries_its_own_form_and_the_incarnations_it_resolved() {
        let db = FakeDb::new();
        account(&db, "alice").await;
        let libs = repository(&db, "libs-a").await;
        repository(&db, "prod").await;

        let issued = issue_scoped(
            &db,
            "alice",
            None,
            &repo_scope("libs-*", &[crate::domain::ScopeAction::Read]),
            &by(),
        )
        .await
        .unwrap();

        assert!(issued.token.starts_with("trgs_"), "{}", issued.token);
        let stored = db.tokens().by_prefix(&issued.prefix).await.unwrap().unwrap();
        assert!(credentials::verify_credential(
            &issued.token,
            &stored.token_hash,
            "trg_"
        ));
        assert_eq!(stored.scope.grants()[0].incarnations, [libs]);
        assert!(
            !credentials::verify_token(&issued.token, &stored.token_hash),
            "a binary without scopes cannot verify it"
        );
    }

    /// Invariant 4: whatever the route and whatever the target, a scoped
    /// credential never makes another credential.
    #[tokio::test]
    async fn a_scoped_caller_issues_nothing() {
        let db = FakeDb::new();
        account(&db, "alice").await;
        let scoped = Actor {
            user_id: Some(1),
            username: "root",
            admin: true,
            scoped: true,
        };

        let refused = refusal(issue_scoped(&db, "alice", None, &TokenScope::Inherit, &scoped).await);

        assert!(matches!(refused, AppError::Forbidden(_)), "{refused:?}");
        assert!(db.tokens().of_user(1).await.unwrap().is_empty());
    }

    /// The vocabulary is closed at creation: a subscription no repository
    /// selector can narrow is not a scope anyone can be issued.
    #[tokio::test]
    async fn a_scope_the_vocabulary_refuses_is_refused_at_creation() {
        let db = FakeDb::new();
        account(&db, "alice").await;
        let webhooks = TokenScope::Limited {
            grants: vec![crate::domain::Grant {
                selector: crate::domain::Selector::Admin {
                    domain: crate::domain::AdminDomain::Webhooks,
                },
                actions: vec![crate::domain::ScopeAction::Write],
                incarnations: Vec::new(),
            }],
        };

        let refused = refusal(issue_scoped(&db, "alice", None, &webhooks, &by()).await);

        assert!(matches!(refused, AppError::BadRequest(_)), "{refused:?}");
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
