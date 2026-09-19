//! `Authenticate`: the one place a credential a client presents is verified,
//! whatever the route and whatever the scheme (A1 C7).
//!
//! Tokens are verified before any limiter is consulted, so a throttled
//! account never blocks its own tokens. Two limiters count failures only:
//! `login:{user}` for passwords, `token:{source}` for tokens.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::auth::rate_limit::RateLimiter;
use crate::auth::{tokens, users as passwords};
use crate::domain::{ApiToken, TokenScope, User};
use crate::error::StoreError;
use crate::ports::clock::Clock;
use crate::ports::signing::{Claims, RegistryTokenSigner};
use crate::ports::tokens::TokenStore;
use crate::ports::users::UserStore;

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub token: String,
    pub user_id: Option<i64>,
    pub username: String,
    pub role: String,
    /// While set, only the password change goes through.
    pub must_change_password: bool,
    /// The API token's display name when one identified the caller.
    pub token_name: Option<String>,
    /// The API token's id, whatever header carried it: the provenance a
    /// derived registry token freezes, and the row a revocation removes.
    /// Rediscovering it by reading a header again is the hole this closes.
    pub api_token_id: Option<String>,
    /// What the presented credential narrows its bearer to.
    pub scope: TokenScope,
}

impl AuthUser {
    fn from_user(user: User, token: &str, credential: Option<&ApiToken>) -> Self {
        Self {
            token: token.to_string(),
            user_id: Some(user.id),
            username: user.username,
            role: user.role,
            must_change_password: user.must_change_password,
            token_name: credential.map(|t| t.name.clone()),
            api_token_id: credential.map(|t| t.id.clone()),
            scope: credential.map_or(TokenScope::Inherit, |t| t.scope.clone()),
        }
    }

    /// The synthetic admin a static config token acts as. A configuration key
    /// is an operations credential, not an identity, so it carries no scope.
    fn static_token(token: &str) -> Self {
        Self {
            token: token.to_string(),
            user_id: None,
            username: "static-token".to_string(),
            role: "admin".to_string(),
            must_change_password: false,
            token_name: None,
            api_token_id: None,
            scope: TokenScope::Inherit,
        }
    }

    fn same_principal(&self, other: &AuthUser) -> bool {
        self.user_id == other.user_id && self.username == other.username
    }
}

pub use crate::domain::CredentialKind;

/// The header a credential arrived in; an adapter declares which one wins
/// when two valid credentials name different principals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Authorization,
    ApiKeyHeader,
}

#[derive(Clone, Debug)]
pub enum Credential {
    Basic { username: String, password: String },
    Bearer(String),
    Registry(String),
    ApiKey(String),
}

#[derive(Clone, Debug)]
pub struct Presented {
    pub transport: Transport,
    pub credential: Credential,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid,
    Throttled,
    Unavailable,
}

impl From<StoreError> for Refusal {
    fn from(err: StoreError) -> Self {
        tracing::warn!(error = %err, "store error during authentication");
        Refusal::Unavailable
    }
}

/// Who a request acts as. `user` is `None` for an anonymous registry token,
/// which is a verified credential naming nobody.
#[derive(Clone, Debug)]
pub struct Authenticated {
    pub user: Option<AuthUser>,
    pub claims: Option<Claims>,
}

/// Whether a verified credential may still act, evaluated after every
/// verification whatever its scheme; a store that cannot answer is a 503.
#[async_trait::async_trait]
pub trait LoginGate: Send + Sync {
    async fn login_allowed(
        &self,
        user: &User,
        kind: CredentialKind,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;
}

pub struct Authenticate {
    static_tokens: Vec<String>,
    token_prefix: String,
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    signer: Arc<dyn RegistryTokenSigner>,
    login_limiter: Arc<RateLimiter>,
    token_limiter: Arc<RateLimiter>,
    gate: Arc<dyn LoginGate>,
    clock: Arc<dyn Clock>,
}

pub struct AuthenticateDeps {
    pub static_tokens: Vec<String>,
    pub token_prefix: String,
    pub users: Arc<dyn UserStore>,
    pub tokens: Arc<dyn TokenStore>,
    pub signer: Arc<dyn RegistryTokenSigner>,
    pub login_limiter: Arc<RateLimiter>,
    pub token_limiter: Arc<RateLimiter>,
    pub gate: Arc<dyn LoginGate>,
    pub clock: Arc<dyn Clock>,
}

impl Authenticate {
    pub fn new(deps: AuthenticateDeps) -> Self {
        Self {
            static_tokens: deps.static_tokens,
            token_prefix: deps.token_prefix,
            users: deps.users,
            tokens: deps.tokens,
            signer: deps.signer,
            login_limiter: deps.login_limiter,
            token_limiter: deps.token_limiter,
            gate: deps.gate,
            clock: deps.clock,
        }
    }

    /// Every presented credential is verified and none is ignored: one
    /// failure refuses the request. Valid credentials naming different
    /// principals resolve to the `primary` transport's, or are refused.
    /// `Ok(None)` is reserved for a request that presented nothing.
    pub async fn run(
        &self,
        presented: &[Presented],
        primary: Option<Transport>,
        source: &str,
    ) -> Result<Option<Authenticated>, Refusal> {
        let mut verified = Vec::with_capacity(presented.len());
        for p in presented {
            verified.push((p.transport, self.one(&p.credential, source).await?));
        }
        let Some((_, first)) = verified.first() else {
            return Ok(None);
        };
        let agree = verified.iter().all(|(_, a)| match (&a.user, &first.user) {
            (Some(x), Some(y)) => x.same_principal(y),
            (None, None) => true,
            _ => false,
        });
        if agree {
            return Ok(Some(first.clone()));
        }
        let chosen = primary.and_then(|t| verified.iter().find(|(tr, _)| *tr == t));
        match chosen {
            Some((_, a)) => Ok(Some(a.clone())),
            None => Err(Refusal::Invalid),
        }
    }

    async fn one(&self, credential: &Credential, source: &str) -> Result<Authenticated, Refusal> {
        match credential {
            Credential::Basic { username, password } => {
                if let Some(user) = self.secret_token(password).await? {
                    return Ok(principal(user));
                }
                if self.is_token_shaped(password) {
                    return Err(self.token_failure(source));
                }
                Ok(principal(self.password(username, password).await?))
            }
            Credential::Bearer(raw) | Credential::ApiKey(raw) => match self.secret_token(raw).await? {
                Some(user) => Ok(principal(user)),
                None => Err(self.token_failure(source)),
            },
            Credential::Registry(raw) => match self.registry_token(raw).await? {
                Some(done) => Ok(done),
                None => Err(self.token_failure(source)),
            },
        }
    }

    /// A password attempt on the account's own limiter, shared by Basic, npm
    /// login and the password change.
    pub async fn password(&self, username: &str, password: &str) -> Result<AuthUser, Refusal> {
        let key = format!("login:{}", username.to_lowercase());
        if self.login_limiter.is_limited(&key) {
            return Err(Refusal::Throttled);
        }
        let user = self.users.by_name(username).await?;
        let verified = match user {
            Some(user) => {
                let ok = passwords::verify_password_async(
                    password.to_string(),
                    user.password_hash.clone(),
                )
                .await
                .unwrap_or(false);
                ok.then_some(user)
            }
            None => None,
        };
        let Some(user) = verified else {
            self.login_limiter.record_failure(&key);
            return Err(Refusal::Invalid);
        };
        if !self.allowed(&user, CredentialKind::Password).await? {
            return Err(Refusal::Invalid);
        }
        Ok(AuthUser::from_user(user, "", None))
    }

    fn token_failure(&self, source: &str) -> Refusal {
        let key = format!("token:{source}");
        if self.token_limiter.is_limited(&key) {
            return Refusal::Throttled;
        }
        self.token_limiter.record_failure(&key);
        Refusal::Invalid
    }

    async fn allowed(&self, user: &User, kind: CredentialKind) -> Result<bool, StoreError> {
        self.gate.login_allowed(user, kind, self.clock.now()).await
    }

    /// A static config token or a stored API token, `None` when it is
    /// neither; never touches a limiter and never runs Argon2.
    async fn secret_token(&self, raw: &str) -> Result<Option<AuthUser>, Refusal> {
        if self.is_static(raw) {
            return Ok(Some(AuthUser::static_token(raw)));
        }
        let Some(stored) = self.live_api_token(raw).await? else {
            return Ok(None);
        };
        let Some(user) = self.users.by_id(stored.user_id).await? else {
            return Ok(None);
        };
        if !self.allowed(&user, CredentialKind::ApiToken).await? {
            return Ok(None);
        }
        let _ = self.tokens.touch(&stored.id, self.clock.now()).await;
        Ok(Some(AuthUser::from_user(user, raw, Some(&stored))))
    }

    /// An API token or a configured static token, by form, before any
    /// lookup.
    pub fn is_token_shaped(&self, raw: &str) -> bool {
        raw.starts_with(&self.token_prefix)
            || tokens::is_scoped_form(raw, &self.token_prefix)
            || self.is_static(raw)
    }

    /// The form the composition root issues under, for the use case that
    /// mints credentials.
    pub fn token_prefix(&self) -> &str {
        &self.token_prefix
    }

    fn is_static(&self, raw: &str) -> bool {
        self.static_tokens
            .iter()
            .any(|st| constant_eq(st.as_bytes(), raw.as_bytes()))
    }

    /// A registry token re-checks the API token it was bought with and the
    /// user it names, then the gate, on every request.
    async fn registry_token(&self, raw: &str) -> Result<Option<Authenticated>, Refusal> {
        let Some(claims) = self.signer.verify(raw) else {
            return Ok(None);
        };
        let mut bought_with = None;
        if let Some(id) = claims.api_token_id.as_deref() {
            match self.live_api_token_by_id(id).await? {
                Some(api_token) => bought_with = Some(api_token),
                None => return Ok(None),
            }
        }
        if claims.static_token {
            let Some(key) = claims.static_key.as_deref() else {
                return Ok(None);
            };
            let live = self
                .static_tokens
                .iter()
                .any(|st| constant_eq(self.signer.fingerprint(st).as_bytes(), key.as_bytes()));
            if !live {
                return Ok(None);
            }
            return Ok(Some(Authenticated {
                user: Some(AuthUser::static_token(raw)),
                claims: Some(claims),
            }));
        }
        let Some(username) = claims.sub.clone() else {
            return Ok(Some(Authenticated {
                user: None,
                claims: Some(claims),
            }));
        };
        let Some(user) = self.users.by_name(&username).await? else {
            return Ok(None);
        };
        if !self.allowed(&user, CredentialKind::RegistryToken).await? {
            return Ok(None);
        }
        Ok(Some(Authenticated {
            user: Some(AuthUser::from_user(user, raw, bought_with.as_ref())),
            claims: Some(claims),
        }))
    }

    /// The API token behind `raw`, if it exists, matches and is still live.
    pub async fn live_api_token(&self, raw: &str) -> Result<Option<ApiToken>, StoreError> {
        if raw.len() < 16 || !raw.is_char_boundary(16) {
            return Ok(None);
        }
        let Some(stored) = self.tokens.by_prefix(&raw[..16]).await? else {
            return Ok(None);
        };
        if !tokens::verify_credential(raw, &stored.token_hash, &self.token_prefix)
            || !stored.is_live(self.clock.now())
        {
            return Ok(None);
        }
        Ok(Some(stored))
    }

    pub async fn live_api_token_by_id(&self, id: &str) -> Result<Option<ApiToken>, StoreError> {
        let stored = self.tokens.by_id(id).await?;
        let now = self.clock.now();
        Ok(stored.filter(|token| token.is_live(now)))
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn principal(user: AuthUser) -> Authenticated {
    Authenticated {
        user: Some(user),
        claims: None,
    }
}

#[cfg(test)]
#[path = "authenticate_tests.rs"]
mod tests;
