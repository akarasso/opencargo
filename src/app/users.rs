//! Administering an account.
//!
//! The password never crosses the boundary in the clear: these use cases hash
//! it before the store is asked for anything, so a failed write leaves no
//! trace of it. A role change is announced as well as recorded — it alters
//! what its holder may do everywhere, and their open sessions have to hear.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::app::audit::{self, Actor};
use crate::app::authenticate::{Authenticate, Refusal};
use crate::auth::users as passwords;
use crate::domain::{Audience, DomainEvent, User};
use crate::error::{AppError, AppResult};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::users::{NewUser, UserPatch, UserStore};

const ROLES: [&str; 3] = ["admin", "publisher", "reader"];

fn valid_role(role: &str) -> AppResult<()> {
    if ROLES.contains(&role) {
        return Ok(());
    }
    Err(AppError::BadRequest(format!("invalid role: {role}")))
}

/// The account, or the 404 naming it. Shared with the token and permission
/// use cases, which answer the same way about the same accounts.
pub(crate) async fn load_account(users: &dyn UserStore, username: &str) -> AppResult<User> {
    users
        .by_name(username)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("user not found: {username}")))
}

/// What an operator asked for. The password is deliberately absent: one is
/// generated here, so no caller can set a weak one at creation time.
pub struct NewAccount<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub role: Option<&'a str>,
}

/// The account, and the one time its password is readable.
pub struct Created {
    pub user: User,
    pub password: String,
}

pub struct CreateUser {
    users: Arc<dyn UserStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl CreateUser {
    pub fn new(
        users: Arc<dyn UserStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            users,
            audit,
            events,
        }
    }

    pub async fn run(
        &self,
        account: &NewAccount<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<Created> {
        if self.users.by_name(account.username).await?.is_some() {
            return Err(AppError::Conflict(format!(
                "user already exists: {}",
                account.username
            )));
        }
        let role = account.role.unwrap_or("reader");
        valid_role(role)?;

        let password = passwords::generate_random_password();
        let password_hash = hashed(&password).await?;
        let user = self
            .users
            .create(
                &NewUser {
                    username: account.username,
                    email: account.email,
                    password_hash: &password_hash,
                    role,
                },
                now,
            )
            .await?;

        audit::record(
            &*self.audit,
            &*self.events,
            by,
            "user.create",
            Some(account.username),
            now,
        )
        .await;
        Ok(Created { user, password })
    }
}

/// Which fields a change touches; `None` leaves one alone.
pub struct AccountPatch<'a> {
    pub email: Option<&'a str>,
    pub password: Option<&'a str>,
    pub role: Option<&'a str>,
}

pub struct UpdateUser {
    users: Arc<dyn UserStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl UpdateUser {
    pub fn new(
        users: Arc<dyn UserStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            users,
            audit,
            events,
        }
    }

    /// The route already let a user through for their own account; changing a
    /// role is the one field that stays the operator's, so it is checked here
    /// rather than in the handler.
    pub async fn run(
        &self,
        username: &str,
        patch: &AccountPatch<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<User> {
        load_account(&*self.users, username).await?;
        if let Some(role) = patch.role {
            if !by.admin {
                return Err(AppError::Forbidden(
                    "only admins can change roles".to_string(),
                ));
            }
            valid_role(role)?;
        }

        let password_hash = match patch.password {
            Some(password) => Some(hashed(password).await?),
            None => None,
        };
        let user = self
            .users
            .update(
                username,
                &UserPatch {
                    email: patch.email,
                    password_hash: password_hash.as_deref(),
                    role: patch.role,
                    ..UserPatch::default()
                },
                now,
            )
            .await?;

        audit::record(
            &*self.audit,
            &*self.events,
            by,
            "user.update",
            Some(username),
            now,
        )
        .await;
        if patch.role.is_some() {
            self.events.emit(
                DomainEvent::PermissionsChanged {
                    username: username.to_string(),
                },
                Audience::Authenticated,
            );
        }
        Ok(user)
    }
}

pub struct DeleteUser {
    users: Arc<dyn UserStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl DeleteUser {
    pub fn new(
        users: Arc<dyn UserStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            users,
            audit,
            events,
        }
    }

    /// The account is read first so a missing one is a 404 rather than the
    /// store's own refusal. Its tokens and grants go with the row; its audit
    /// trail is meant to outlive it and stays.
    pub async fn run(&self, username: &str, by: &Actor<'_>, now: DateTime<Utc>) -> AppResult<()> {
        load_account(&*self.users, username).await?;
        self.users.delete(username).await?;

        audit::record(
            &*self.audit,
            &*self.events,
            by,
            "user.delete",
            Some(username),
            now,
        )
        .await;
        Ok(())
    }
}

/// Changing one's own password, which is neither an administrative action nor
/// something the trail records: the caller proves they know the current one
/// unless an operator is acting for them.
/// The current password goes through [`Authenticate`], so a throttled
/// account answers 429 here as it does on Basic and npm login.
pub struct ChangePassword {
    users: Arc<dyn UserStore>,
    authenticate: Arc<Authenticate>,
}

impl ChangePassword {
    pub fn new(users: Arc<dyn UserStore>, authenticate: Arc<Authenticate>) -> Self {
        Self {
            users,
            authenticate,
        }
    }

    pub async fn run(
        &self,
        username: &str,
        current: Option<&str>,
        new: &str,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let user = load_account(&*self.users, username).await?;
        if !by.admin {
            let current = current.ok_or_else(|| {
                AppError::BadRequest("current_password is required".to_string())
            })?;
            match self.authenticate.password(&user.username, current).await {
                Ok(_) => {}
                Err(Refusal::Invalid) => {
                    return Err(AppError::Unauthorized(
                        "invalid current password".to_string(),
                    ))
                }
                Err(Refusal::Throttled) => {
                    return Err(AppError::TooManyRequests(
                        "too many authentication attempts, try again later".to_string(),
                    ))
                }
                Err(Refusal::Unavailable) => {
                    return Err(AppError::ServiceUnavailable(
                        "authentication temporarily unavailable, try again".to_string(),
                    ))
                }
            }
        }

        let password_hash = hashed(new).await?;
        self.users
            .update(
                username,
                &UserPatch {
                    password_hash: Some(&password_hash),
                    must_change_password: Some(false),
                    ..UserPatch::default()
                },
                now,
            )
            .await?;
        Ok(())
    }
}

async fn hashed(password: &str) -> AppResult<String> {
    passwords::hash_password_async(password.to_string())
        .await
        .map_err(|e| AppError::Internal(format!("failed to hash password: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fakes::FakeDb;

    fn by(admin: bool) -> Actor<'static> {
        Actor {
            user_id: Some(1),
            username: "root",
            admin,
            scoped: false,
        }
    }

    async fn seeded(db: &FakeDb) -> Created {
        CreateUser::new(db.users(), db.audit(), crate::server::event_bus())
            .run(
                &NewAccount {
                    username: "alice",
                    email: None,
                    role: Some("reader"),
                },
                &by(true),
                Utc::now(),
            )
            .await
            .unwrap()
    }

    /// The generated password is the one that was stored, and it is handed
    /// back once rather than kept anywhere.
    #[tokio::test]
    async fn a_created_account_is_reachable_with_the_password_it_returned() {
        let db = FakeDb::new();
        let created = seeded(&db).await;

        let stored = db.users().by_name("alice").await.unwrap().unwrap();
        assert!(
            passwords::verify_password_async(created.password, stored.password_hash)
                .await
                .unwrap()
        );
        assert_eq!(db.audit_rows().len(), 1);
    }

    /// The role is the operator's field. A self-service caller is refused
    /// before the store is touched, so nothing else in the patch lands either.
    #[tokio::test]
    async fn a_non_admin_cannot_change_a_role() {
        let db = FakeDb::new();
        seeded(&db).await;

        let refused = UpdateUser::new(db.users(), db.audit(), crate::server::event_bus())
            .run(
                "alice",
                &AccountPatch {
                    email: Some("alice@example.com"),
                    password: None,
                    role: Some("admin"),
                },
                &by(false),
                Utc::now(),
            )
            .await;

        assert!(matches!(refused, Err(AppError::Forbidden(_))), "{refused:?}");
        let stored = db.users().by_name("alice").await.unwrap().unwrap();
        assert_eq!(stored.role, "reader");
        assert_eq!(stored.email, None);
    }

    /// An account that is not there is a 404, not the store's refusal.
    #[tokio::test]
    async fn deleting_an_unknown_account_names_it() {
        let db = FakeDb::new();
        let refused = DeleteUser::new(db.users(), db.audit(), crate::server::event_bus())
            .run("nobody", &by(true), Utc::now())
            .await
            .unwrap_err();

        assert!(matches!(refused, AppError::NotFound(_)), "{refused:?}");
        assert!(db.audit_rows().is_empty());
    }
}
