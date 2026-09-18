//! The accounts an operator administers, and the one the auth path resolves
//! a credential to.
//!
//! Nothing here is coarse: an account is one row, and the only write that
//! touches a second table is deleting one, which the schema cascades.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::User;
use crate::error::StoreError;

/// An account to create: borrowed in, owned out.
pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub password_hash: &'a str,
    pub role: &'a str,
}

/// Which fields an update touches; `None` leaves one alone. The forced
/// password rotation is a field like any other, so a password change that
/// clears it is one write rather than two.
#[derive(Default)]
pub struct UserPatch<'a> {
    pub email: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub role: Option<&'a str>,
    pub must_change_password: Option<bool>,
}

impl UserPatch<'_> {
    pub fn touches_nothing(&self) -> bool {
        self.email.is_none()
            && self.password_hash.is_none()
            && self.role.is_none()
            && self.must_change_password.is_none()
    }
}

#[async_trait]
pub trait UserStore: Send + Sync {
    /// Case-sensitive, like the column's UNIQUE constraint.
    async fn by_name(&self, username: &str) -> Result<Option<User>, StoreError>;

    /// The lookup a credential reaches an account through: a token names its
    /// user by id, never by name.
    async fn by_id(&self, id: i64) -> Result<Option<User>, StoreError>;

    async fn all(&self) -> Result<Vec<User>, StoreError>;

    /// `Conflict` if the username is taken. `now` fills `created_at` and
    /// `updated_at`, so no column default ever fires.
    async fn create(&self, user: &NewUser<'_>, now: DateTime<Utc>) -> Result<User, StoreError>;

    /// `NotFound` if no such account; a patch that touches nothing reads the
    /// account back without stamping it.
    async fn update(
        &self,
        username: &str,
        patch: &UserPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError>;

    /// `NotFound` if it was already gone. The account's tokens and grants go
    /// with it; its audit trail does not.
    async fn delete(&self, username: &str) -> Result<(), StoreError>;
}
