//! The gate `Authenticate` consults after every verification: the password
//! mode, the disabled state and SSO's `reauth_after`, decided by the domain's
//! `login_allowed` over what `IdentityStore` knows of the account.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::app::authenticate::LoginGate;
use crate::domain::identity::{login_allowed, GatePolicy};
use crate::domain::{CredentialKind, User};
use crate::error::StoreError;
use crate::ports::identities::IdentityStore;

pub struct PolicyGate {
    identities: Arc<dyn IdentityStore>,
    policy: GatePolicy,
    bootstrap: Option<String>,
}

impl PolicyGate {
    /// `bootstrap` names the configured admin account, which is never locked.
    pub fn new(
        identities: Arc<dyn IdentityStore>,
        policy: GatePolicy,
        bootstrap: Option<String>,
    ) -> Self {
        Self {
            identities,
            policy,
            bootstrap: bootstrap.filter(|b| !b.is_empty()),
        }
    }
}

#[async_trait]
impl LoginGate for PolicyGate {
    async fn login_allowed(
        &self,
        user: &User,
        kind: CredentialKind,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let mut state = self.identities.login_state(user.id).await?;
        state.bootstrap = self.bootstrap.as_deref() == Some(user.username.as_str());
        Ok(login_allowed(&user.role, &state, kind, now, &self.policy))
    }
}
