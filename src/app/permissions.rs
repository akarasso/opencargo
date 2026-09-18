//! Granting and withdrawing what a user may do on a repository.
//!
//! Both resolve the account and the repository first, so an operator naming
//! either wrongly is told which one rather than silently writing a grant
//! against an id that means nothing. The announcement carries only the
//! username: the rights themselves stay behind the REST API, which filters
//! per caller.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::app::audit::{self, Actor};
use crate::app::users::load_account;
use crate::domain::{Audience, DomainEvent, Rights};
use crate::error::AppResult;
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::permissions::PermissionStore;
use crate::ports::repositories::RepositoryStore;
use crate::ports::users::UserStore;
use crate::registry::load_repo;

/// What the two use cases share: the pair of ids a grant is keyed on, and the
/// trail plus the refresh hint every change leaves.
struct Grants {
    users: Arc<dyn UserStore>,
    repos: Arc<dyn RepositoryStore>,
    permissions: Arc<dyn PermissionStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl Grants {
    async fn subject(&self, username: &str, repository: &str) -> AppResult<(i64, i64)> {
        let user = load_account(&*self.users, username).await?;
        let repo = load_repo(self.repos.as_ref(), repository).await?;
        Ok((user.id, repo.id))
    }

    async fn recorded(
        &self,
        by: &Actor<'_>,
        action: &str,
        username: &str,
        repository: &str,
        now: DateTime<Utc>,
    ) {
        let target = format!("{username} on {repository}");
        audit::record(&*self.audit, &*self.events, by, action, Some(&target), now).await;
        self.events.emit(
            DomainEvent::PermissionsChanged {
                username: username.to_string(),
            },
            Audience::Authenticated,
        );
    }
}

pub struct SetPermission(Grants);

impl SetPermission {
    pub fn new(
        users: Arc<dyn UserStore>,
        repos: Arc<dyn RepositoryStore>,
        permissions: Arc<dyn PermissionStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self(Grants {
            users,
            repos,
            permissions,
            audit,
            events,
        })
    }

    pub async fn run(
        &self,
        username: &str,
        repository: &str,
        rights: Rights,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let (user_id, repo_id) = self.0.subject(username, repository).await?;
        self.0.permissions.set(user_id, repo_id, rights, now).await?;
        self.0
            .recorded(by, "permission.set", username, repository, now)
            .await;
        Ok(())
    }
}

pub struct RevokePermission(Grants);

impl RevokePermission {
    pub fn new(
        users: Arc<dyn UserStore>,
        repos: Arc<dyn RepositoryStore>,
        permissions: Arc<dyn PermissionStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self(Grants {
            users,
            repos,
            permissions,
            audit,
            events,
        })
    }

    pub async fn run(
        &self,
        username: &str,
        repository: &str,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let (user_id, repo_id) = self.0.subject(username, repository).await?;
        self.0.permissions.revoke(user_id, repo_id).await?;
        self.0
            .recorded(by, "permission.remove", username, repository, now)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::repositories::CreateRepository;
    use crate::app::users::{CreateUser, NewAccount};
    use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
    use crate::error::AppError;
    use crate::testing::fakes::FakeDb;

    fn by() -> Actor<'static> {
        Actor {
            user_id: Some(1),
            username: "root",
            admin: true,
        }
    }

    fn rights() -> Rights {
        Rights {
            read: true,
            write: true,
            delete: false,
            admin: false,
        }
    }

    async fn world(db: &FakeDb) {
        CreateUser::new(db.users(), db.audit(), crate::server::event_bus())
            .run(
                &NewAccount {
                    username: "alice",
                    email: None,
                    role: Some("reader"),
                },
                &by(),
                Utc::now(),
            )
            .await
            .unwrap();
        CreateRepository::new(db.repositories(), db.audit(), crate::server::event_bus())
            .run(
                &RepoSpec {
                    name: "npm-hosted",
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility: Visibility::Private,
                    upstream: None,
                    members: &[],
                },
                &by(),
                Utc::now(),
            )
            .await
            .unwrap();
    }

    fn setter(db: &FakeDb) -> SetPermission {
        SetPermission::new(
            db.users(),
            db.repositories(),
            db.perms(),
            db.audit(),
            crate::server::event_bus(),
        )
    }

    #[tokio::test]
    async fn a_grant_lands_against_the_pair_of_ids_it_names() {
        let db = FakeDb::new();
        world(&db).await;

        setter(&db)
            .run("alice", "npm-hosted", rights(), &by(), Utc::now())
            .await
            .unwrap();

        let user = db.users().by_name("alice").await.unwrap().unwrap();
        let repo = db.repositories().by_name("npm-hosted").await.unwrap().unwrap();
        assert_eq!(
            db.perms().rights(user.id, repo.id).await.unwrap(),
            Some(rights())
        );
    }

    /// A repository nobody has is named in the refusal, and no grant is
    /// written against an id that means nothing.
    #[tokio::test]
    async fn an_unknown_repository_is_refused_before_the_grant() {
        let db = FakeDb::new();
        world(&db).await;

        let refused = setter(&db)
            .run("alice", "nope", rights(), &by(), Utc::now())
            .await
            .unwrap_err();

        assert!(matches!(refused, AppError::NotFound(_)), "{refused:?}");
        let user = db.users().by_name("alice").await.unwrap().unwrap();
        assert!(db.perms().of_user(user.id).await.unwrap().is_empty());
    }

    /// Revoking is idempotent in the store, and still records that it was
    /// asked for.
    #[tokio::test]
    async fn revoking_a_grant_that_is_not_there_is_recorded_all_the_same() {
        let db = FakeDb::new();
        world(&db).await;

        RevokePermission::new(
            db.users(),
            db.repositories(),
            db.perms(),
            db.audit(),
            crate::server::event_bus(),
        )
        .run("alice", "npm-hosted", &by(), Utc::now())
        .await
        .unwrap();

        let actions: Vec<String> = db.audit_rows().into_iter().map(|row| row.1).collect();
        assert!(
            actions.contains(&"permission.remove".to_string()),
            "{actions:?}"
        );
    }
}
