//! Port 19 (A1 C1): external identities, their links to users, the
//! provenance of the credentials they produced and the provider's
//! reachability as the server's own probe saw it. Every write is one
//! transaction, and every state change that ends a credential revokes the
//! credentials derived from it in that same transaction.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::identity::{Authority, IdentityKey, LoginState, Outage};
use crate::domain::{Rights, User};
use crate::error::StoreError;
use crate::ports::users::NewUser;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityLink {
    pub key: IdentityKey,
    pub user_id: i64,
    pub email: Option<String>,
    /// The account was created by this identity rather than linked to it.
    pub provisioned: bool,
    pub disabled: bool,
    pub linked_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
}

/// A login of a known identity that the rules let in: the role and grants
/// they give, applied with the login's bookkeeping in one transaction.
pub struct Admission<'a> {
    pub key: &'a IdentityKey,
    pub email: Option<&'a str>,
    /// Applied only to an account this identity provisioned.
    pub role: &'a str,
    pub grants: &'a [(i64, Rights)],
    /// Every repository the provider's rules name: a grant on one of them
    /// that is not in `grants` any more is revoked.
    pub managed: &'a [i64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisabledBy {
    Admin,
    Denied,
}

impl DisabledBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Denied => "denied",
        }
    }
}

#[async_trait]
pub trait IdentityStore: Send + Sync {
    async fn find(&self, key: &IdentityKey) -> Result<Option<IdentityLink>, StoreError>;

    async fn of_user(&self, user_id: i64) -> Result<Vec<IdentityLink>, StoreError>;

    /// A new account and its link. `Conflict` if the name is taken.
    async fn provision(
        &self,
        user: &NewUser<'_>,
        admission: &Admission<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError>;

    /// `NotFound` without a live link. Lifts a disablement the rules made,
    /// never one an admin made.
    async fn admit(
        &self,
        admission: &Admission<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError>;

    /// Links an existing account, remembering its role. `Conflict` if the
    /// identity is linked already.
    async fn attach(
        &self,
        user_id: i64,
        key: &IdentityKey,
        email: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Removes the link, revokes what it produced and restores the role the
    /// account had before it. `NotFound` if it was not linked.
    async fn detach(&self, user_id: i64, key: &IdentityKey) -> Result<(), StoreError>;

    /// `NotFound` if it was not linked.
    async fn disable_link(&self, key: &IdentityKey) -> Result<(), StoreError>;

    /// Disables the account and revokes every credential its identities
    /// produced.
    async fn disable_user(
        &self,
        user_id: i64,
        by: DisabledBy,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn enable_user(&self, user_id: i64) -> Result<(), StoreError>;

    /// A provider's rules deny a known identity: its account is disabled and
    /// its derived credentials revoked.
    async fn deprovision(&self, key: &IdentityKey, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// A retired provider: every derived credential revoked, every link
    /// disabled. Returns the number of credentials revoked.
    async fn revoke_authority(&self, authority: &Authority) -> Result<u64, StoreError>;

    /// A declared issuer migration: links and provenance move over.
    async fn migrate_authority(&self, from: &Authority, to: &Authority) -> Result<(), StoreError>;

    async fn authorities(&self) -> Result<Vec<Authority>, StoreError>;

    /// Records which identity a token was issued under.
    async fn mark_provenance(&self, token_id: &str, key: &IdentityKey) -> Result<(), StoreError>;

    async fn provenance(&self, token_id: &str) -> Result<Option<IdentityKey>, StoreError>;

    /// What `login_allowed` needs about one account, bootstrap aside.
    async fn login_state(&self, user_id: i64) -> Result<LoginState, StoreError>;

    /// One result of the server's own probe of a provider: a failure opens
    /// an outage, a success closes it. Nothing a client can cause feeds it.
    async fn record_probe(
        &self,
        authority: &Authority,
        reachable: bool,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn outages(&self, authority: &Authority) -> Result<Vec<Outage>, StoreError>;
}
