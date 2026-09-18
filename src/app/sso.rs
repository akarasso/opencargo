//! SSO use cases: begin and complete a login or a link, exchange a handoff
//! for a session token, confirm a link, log out, detach and disable, and the
//! startup reconciliation of configured providers against known ones.
//!
//! Every network call to a provider happens before any write, and nothing is
//! written before the attempt's cookie and `state` match. A handoff is
//! consumed by compare-and-consume, never by racing its expiry.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::app::authenticate::{Authenticate, Refusal};
use crate::auth::rate_limit::RateLimiter;
use crate::auth::seal::Sealer;
use crate::auth::tokens as credentials;
use crate::domain::identity::{
    self, email_link_eligible, provisioned_name, safe_return_to, Authority, Decision, EmailTrust,
    ExternalIdentity, IdentityKey, Reconcile, SsoRefusal,
};
use crate::domain::{can_admin, Rights, User, Visibility};
use crate::error::StoreError;
use crate::ports::audit::{AuditStore, NewAuditEntry};
use crate::ports::clock::Clock;
use crate::ports::handoffs::{Consumption, LoginHandoffStore, NewHandoff};
use crate::ports::identities::{Admission, DisabledBy, IdentityStore};
use crate::ports::identity_provider::{Attempt, Callback, IdentityProvider, IdpFailure};
use crate::ports::ids::Ids;
use crate::ports::repositories::RepositoryStore;
use crate::ports::tokens::{NewToken, TokenStore};
use crate::ports::users::{NewUser, UserStore};

const ATTEMPT_PURPOSE: &str = "sso-attempt";
const TOKEN_PREFIX: &str = "trg_";
/// No password verifies against this: an account SSO created has none.
const NO_PASSWORD: &str = "!sso";

#[derive(Debug)]
pub enum SsoError {
    Refused(SsoRefusal),
    Credentials(Refusal),
    Store(StoreError),
}

impl From<StoreError> for SsoError {
    fn from(e: StoreError) -> Self {
        SsoError::Store(e)
    }
}

impl From<SsoRefusal> for SsoError {
    fn from(r: SsoRefusal) -> Self {
        SsoError::Refused(r)
    }
}

pub type SsoResult<T> = Result<T, SsoError>;

pub struct SsoSettings {
    pub base_url: String,
    pub session_ttl: Duration,
    pub handoff_ttl: Duration,
    pub attempt_ttl: Duration,
    /// The configured admin account, never linked nor locked.
    pub bootstrap: Option<String>,
}

pub struct SsoDeps {
    pub providers: Vec<Arc<dyn IdentityProvider>>,
    pub identities: Arc<dyn IdentityStore>,
    pub handoffs: Arc<dyn LoginHandoffStore>,
    pub users: Arc<dyn UserStore>,
    pub tokens: Arc<dyn TokenStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub audit: Arc<dyn AuditStore>,
    pub ids: Arc<dyn Ids>,
    pub clock: Arc<dyn Clock>,
    pub authenticate: Arc<Authenticate>,
    pub sealer: Arc<Sealer>,
    pub settings: SsoSettings,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Pending {
    Login { attempt: Attempt, return_to: String },
    Link { attempt: Attempt, user_id: i64 },
}

impl Pending {
    fn attempt(&self) -> &Attempt {
        match self {
            Pending::Login { attempt, .. } | Pending::Link { attempt, .. } => attempt,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    provider: String,
    issuer: String,
    subject: String,
}

impl From<&IdentityKey> for StoredKey {
    fn from(k: &IdentityKey) -> Self {
        Self {
            provider: k.authority.provider.clone(),
            issuer: k.authority.issuer.clone(),
            subject: k.subject.clone(),
        }
    }
}

impl StoredKey {
    fn key(&self) -> IdentityKey {
        IdentityKey {
            authority: Authority {
                provider: self.provider.clone(),
                issuer: self.issuer.clone(),
            },
            subject: self.subject.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HandoffPayload {
    Login {
        user_id: i64,
        key: StoredKey,
        return_to: String,
    },
    Link {
        user_id: i64,
        target: String,
        key: StoredKey,
        email: Option<String>,
        email_verified: bool,
        proposal: bool,
    },
}

/// Where to send the browser, and the attempt cookie to set.
pub struct Begun {
    pub location: String,
    pub cookie: String,
}

/// The SPA path the callback lands on; the handoff code rides in its
/// fragment, which no server log sees.
pub struct Landed {
    pub location: String,
}

pub struct Session {
    pub token: String,
    pub username: String,
    pub expires_at: DateTime<Utc>,
    pub return_to: String,
}

/// What the confirmation screen shows before a human confirms a link.
#[derive(Clone, Debug, Serialize)]
pub struct LinkProposal {
    pub provider: String,
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    /// The address may be offered as the reason for the link (decision 9).
    pub proposal: bool,
    pub target: String,
}

#[derive(Clone, Copy, Default)]
pub struct Meta<'a> {
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

pub struct Sso {
    providers: Vec<Arc<dyn IdentityProvider>>,
    identities: Arc<dyn IdentityStore>,
    handoffs: Arc<dyn LoginHandoffStore>,
    users: Arc<dyn UserStore>,
    tokens: Arc<dyn TokenStore>,
    repos: Arc<dyn RepositoryStore>,
    audit: Arc<dyn AuditStore>,
    ids: Arc<dyn Ids>,
    clock: Arc<dyn Clock>,
    authenticate: Arc<Authenticate>,
    sealer: Arc<Sealer>,
    settings: SsoSettings,
    forged: RateLimiter,
}

fn random_code() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(value: &str) -> String {
    sha2::Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn refusal_of(consumption: &Consumption) -> SsoRefusal {
    match consumption {
        Consumption::AlreadyConsumed => SsoRefusal::HandoffConsumed,
        Consumption::Expired => SsoRefusal::HandoffExpired,
        Consumption::BindingMismatch => SsoRefusal::BindingMismatch,
        Consumption::Unknown | Consumption::Consumed(_) => SsoRefusal::HandoffUnknown,
    }
}

fn idp_refusal(failure: &IdpFailure) -> SsoRefusal {
    match failure {
        IdpFailure::Unavailable(_) => SsoRefusal::Unavailable,
        IdpFailure::Rejected(_) => SsoRefusal::Rejected,
        IdpFailure::IdpError(_) => SsoRefusal::IdpError,
    }
}

impl Sso {
    pub fn new(deps: SsoDeps) -> Self {
        Self {
            providers: deps.providers,
            identities: deps.identities,
            handoffs: deps.handoffs,
            users: deps.users,
            tokens: deps.tokens,
            repos: deps.repos,
            audit: deps.audit,
            ids: deps.ids,
            clock: deps.clock,
            authenticate: deps.authenticate,
            sealer: deps.sealer,
            settings: deps.settings,
            forged: RateLimiter::new(20, 60),
        }
    }

    /// The configured providers' names, for the login screen.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|p| p.profile().authority.provider.clone())
            .collect()
    }

    fn provider(&self, name: &str) -> SsoResult<&Arc<dyn IdentityProvider>> {
        self.providers
            .iter()
            .find(|p| p.profile().authority.provider == name)
            .ok_or(SsoError::Refused(SsoRefusal::UnknownProvider))
    }

    pub fn redirect_uri(&self, provider: &str) -> String {
        format!(
            "{}/api/v1/auth/sso/{provider}/callback",
            self.settings.base_url.trim_end_matches('/')
        )
    }

    fn is_bootstrap(&self, user: &User) -> bool {
        self.settings.bootstrap.as_deref() == Some(user.username.as_str())
    }

    async fn record(
        &self,
        user: Option<&User>,
        action: &str,
        target: Option<&str>,
        detail: Option<&str>,
        meta: Meta<'_>,
    ) {
        let details = detail.map(|d| serde_json::json!({ "reason": d }).to_string());
        let entry = NewAuditEntry {
            user_id: user.map(|u| u.id),
            username: user.map(|u| u.username.as_str()),
            action,
            target,
            repository: None,
            ip: meta.ip,
            user_agent: meta.user_agent,
            details_json: details.as_deref(),
        };
        if let Err(e) = self.audit.append(&entry, self.clock.now()).await {
            tracing::warn!(error = %e, action, "failed to write an SSO audit entry");
        }
    }

    async fn begin(
        &self,
        provider: &str,
        pending: impl FnOnce(Attempt) -> Pending,
    ) -> SsoResult<Begun> {
        let p = self.provider(provider)?;
        let (location, attempt) = p
            .start(&self.redirect_uri(provider))
            .await
            .map_err(|e| SsoError::Refused(idp_refusal(&e)))?;
        let now = self.clock.now();
        let cookie = self.sealer.seal(
            ATTEMPT_PURPOSE,
            &pending(attempt),
            now + self.settings.attempt_ttl,
        );
        Ok(Begun { location, cookie })
    }

    /// Writes nothing: the attempt lives in the cookie it returns.
    pub async fn begin_login(&self, provider: &str, return_to: &str) -> SsoResult<Begun> {
        let return_to = safe_return_to(return_to).to_string();
        self.begin(provider, |attempt| Pending::Login { attempt, return_to })
            .await
    }

    /// From a live session and the current password; never for an admin or
    /// the bootstrap account.
    pub async fn begin_link(
        &self,
        user_id: i64,
        password: &str,
        provider: &str,
    ) -> SsoResult<Begun> {
        let user = self
            .users
            .by_id(user_id)
            .await?
            .ok_or(SsoError::Refused(SsoRefusal::WrongUser))?;
        if can_admin(&user.role) || self.is_bootstrap(&user) {
            return Err(SsoRefusal::AdminNotLinkable.into());
        }
        self.provider(provider)?;
        self.authenticate
            .password(&user.username, password)
            .await
            .map_err(SsoError::Credentials)?;
        self.begin(provider, |attempt| Pending::Link { attempt, user_id })
            .await
    }

    /// The provider's redirect back. The cookie and `state` are checked
    /// before anything is written; a forged callback leaves at most an audit
    /// row, and those are capped per provider.
    pub async fn complete(
        &self,
        provider: &str,
        callback: &Callback,
        cookie: Option<&str>,
        meta: Meta<'_>,
    ) -> SsoResult<Landed> {
        let now = self.clock.now();
        let pending: Option<Pending> =
            cookie.and_then(|c| self.sealer.open(ATTEMPT_PURPOSE, c, now));
        let matched = pending.as_ref().is_some_and(|p| {
            p.attempt().provider == provider
                && callback
                    .state
                    .as_deref()
                    .is_some_and(|s| constant_eq(s, &p.attempt().state))
        });
        let (Some(pending), Some(cookie), true) = (pending, cookie, matched) else {
            let key = format!("forged:{provider}");
            if !self.forged.is_limited(&key) {
                self.forged.record_failure(&key);
                self.record(
                    None,
                    "sso.callback.refused",
                    Some(provider),
                    Some("state_mismatch"),
                    meta,
                )
                .await;
            }
            return Err(SsoRefusal::StateMismatch.into());
        };
        let p = self.provider(provider)?.clone();
        let identity = match p
            .finish(pending.attempt(), callback, &self.redirect_uri(provider))
            .await
        {
            Ok(identity) => identity,
            Err(failure) => {
                let refusal = idp_refusal(&failure);
                self.record(
                    None,
                    "sso.callback.refused",
                    Some(provider),
                    Some(&failure.to_string()),
                    meta,
                )
                .await;
                return Err(refusal.into());
            }
        };
        let binding = digest(cookie);
        match pending {
            Pending::Login { return_to, .. } => {
                self.login(p.as_ref(), identity, &return_to, &binding, meta)
                    .await
            }
            Pending::Link { user_id, .. } => {
                self.propose_link(p.as_ref(), identity, user_id, &binding, meta)
                    .await
            }
        }
    }

    async fn grants_for(
        &self,
        p: &dyn IdentityProvider,
        grants: &[(String, identity::GrantRole)],
    ) -> SsoResult<(Vec<(i64, Rights)>, Vec<i64>)> {
        let mut managed = Vec::new();
        let mut given = Vec::new();
        for rule in &p.profile().policy.grants {
            let Some(repo) = self.repos.by_name(&rule.repository).await? else {
                continue;
            };
            if repo.visibility != Visibility::Private {
                continue;
            }
            if !managed.contains(&repo.id) {
                managed.push(repo.id);
            }
            if let Some((_, role)) = grants.iter().find(|(name, _)| name == &repo.name) {
                let rights = role.rights();
                match given.iter_mut().find(|(id, _)| *id == repo.id) {
                    Some((_, r)) if rights.write => *r = rights,
                    Some(_) => {}
                    None => given.push((repo.id, rights)),
                }
            }
        }
        Ok((given, managed))
    }

    async fn login(
        &self,
        p: &dyn IdentityProvider,
        identity: ExternalIdentity,
        return_to: &str,
        binding: &str,
        meta: Meta<'_>,
    ) -> SsoResult<Landed> {
        let target = format!(
            "{}:{}",
            identity.key.authority.provider, identity.key.subject
        );
        let link = self.identities.find(&identity.key).await?;
        let decision = match p.profile().policy.decide(&identity) {
            Ok(d) => d,
            Err(_) => {
                self.record(
                    None,
                    "sso.login.rejected",
                    Some(&target),
                    Some("groups_unreadable"),
                    meta,
                )
                .await;
                return Err(SsoRefusal::Rejected.into());
            }
        };
        if link.as_ref().is_some_and(|l| l.disabled) {
            self.record(
                None,
                "sso.login.refused",
                Some(&target),
                Some("disabled"),
                meta,
            )
            .await;
            return Err(SsoRefusal::Disabled.into());
        }
        let (role, grants) = match decision {
            Decision::Denied(reason) => {
                if link.is_some() {
                    self.identities
                        .deprovision(&identity.key, self.clock.now())
                        .await?;
                }
                self.record(
                    None,
                    "sso.login.denied",
                    Some(&target),
                    Some(&format!("{reason:?}")),
                    meta,
                )
                .await;
                return Err(SsoRefusal::Denied.into());
            }
            Decision::Access { role, grants } => (role, grants),
        };
        let (given, managed) = self.grants_for(p, &grants).await?;
        let admission = Admission {
            key: &identity.key,
            email: identity.email.as_deref(),
            role: &role,
            grants: &given,
            managed: &managed,
        };
        let now = self.clock.now();
        let user = if link.is_some() {
            self.identities.admit(&admission, now).await?
        } else {
            let Some(name) = provisioned_name(&identity) else {
                return Err(SsoRefusal::NameCollision.into());
            };
            let new = NewUser {
                username: &name,
                email: identity.email.as_deref(),
                password_hash: NO_PASSWORD,
                role: &role,
            };
            match self.identities.provision(&new, &admission, now).await {
                Ok(user) => user,
                Err(StoreError::Conflict) => {
                    self.record(
                        None,
                        "sso.login.refused",
                        Some(&target),
                        Some("name_collision"),
                        meta,
                    )
                    .await;
                    return Err(SsoRefusal::NameCollision.into());
                }
                Err(e) => return Err(e.into()),
            }
        };
        if self.identities.login_state(user.id).await?.disabled {
            self.record(
                Some(&user),
                "sso.login.refused",
                Some(&target),
                Some("disabled"),
                meta,
            )
            .await;
            return Err(SsoRefusal::Disabled.into());
        }
        let payload = HandoffPayload::Login {
            user_id: user.id,
            key: (&identity.key).into(),
            return_to: return_to.to_string(),
        };
        let code = self.deposit(&payload, binding).await?;
        Ok(Landed {
            location: format!("/login/sso/complete#code={code}"),
        })
    }

    async fn propose_link(
        &self,
        p: &dyn IdentityProvider,
        identity: ExternalIdentity,
        user_id: i64,
        binding: &str,
        meta: Meta<'_>,
    ) -> SsoResult<Landed> {
        let target = self
            .users
            .by_id(user_id)
            .await?
            .ok_or(SsoError::Refused(SsoRefusal::WrongUser))?;
        if can_admin(&target.role) || self.is_bootstrap(&target) {
            return Err(SsoRefusal::AdminNotLinkable.into());
        }
        if self.identities.find(&identity.key).await?.is_some() {
            self.record(
                Some(&target),
                "sso.link.refused",
                Some(&identity.key.subject),
                Some("already_linked"),
                meta,
            )
            .await;
            return Err(SsoRefusal::AlreadyLinked.into());
        }
        let payload = HandoffPayload::Link {
            user_id,
            target: target.username.clone(),
            key: (&identity.key).into(),
            email: identity.email.clone(),
            email_verified: identity.email_trust == EmailTrust::Verified,
            proposal: email_link_eligible(p.profile(), &identity),
        };
        let code = self.deposit(&payload, binding).await?;
        Ok(Landed {
            location: format!("/login/sso/link#code={code}"),
        })
    }

    async fn deposit(&self, payload: &HandoffPayload, binding: &str) -> SsoResult<String> {
        let code = random_code();
        let payload = serde_json::to_string(payload).expect("a handoff serializes");
        self.handoffs
            .deposit(&NewHandoff {
                code_hash: &digest(&code),
                binding,
                payload: &payload,
                expires_at: self.clock.now() + self.settings.handoff_ttl,
            })
            .await?;
        Ok(code)
    }

    /// Consumes the handoff whatever the verdict; every refusal is audited.
    async fn consume(
        &self,
        code: &str,
        cookie: Option<&str>,
        meta: Meta<'_>,
    ) -> SsoResult<HandoffPayload> {
        let binding = digest(cookie.unwrap_or(""));
        let consumption = self
            .handoffs
            .consume(&digest(code), &binding, self.clock.now())
            .await?;
        let Consumption::Consumed(payload) = consumption else {
            let refusal = refusal_of(&consumption);
            self.record(
                None,
                "sso.handoff.refused",
                None,
                Some(refusal.as_str()),
                meta,
            )
            .await;
            return Err(refusal.into());
        };
        serde_json::from_str(&payload).map_err(|_| SsoError::Refused(SsoRefusal::HandoffUnknown))
    }

    /// The SPA's POST: a login handoff for an ordinary API token that
    /// carries its provenance.
    pub async fn exchange(
        &self,
        code: &str,
        cookie: Option<&str>,
        meta: Meta<'_>,
    ) -> SsoResult<Session> {
        let HandoffPayload::Login {
            user_id,
            key,
            return_to,
        } = self.consume(code, cookie, meta).await?
        else {
            return Err(SsoRefusal::HandoffUnknown.into());
        };
        let user = self
            .users
            .by_id(user_id)
            .await?
            .ok_or(SsoError::Refused(SsoRefusal::Disabled))?;
        if self.identities.login_state(user.id).await?.disabled {
            return Err(SsoRefusal::Disabled.into());
        }
        let now = self.clock.now();
        let id = self.ids.token_id();
        let (token, hash) = credentials::generate_token(TOKEN_PREFIX);
        let expires_at = now + self.settings.session_ttl;
        self.tokens
            .create(
                &NewToken {
                    id: &id,
                    user_id: user.id,
                    name: &format!("sso:{}", key.provider),
                    prefix: &token[..16],
                    token_hash: &hash,
                    expires_at: Some(expires_at),
                },
                now,
            )
            .await?;
        if let Err(e) = self.identities.mark_provenance(&id, &key.key()).await {
            let _ = self.tokens.delete(&id).await;
            return Err(e.into());
        }
        self.record(Some(&user), "sso.login", Some(&key.provider), None, meta)
            .await;
        Ok(Session {
            token,
            username: user.username,
            expires_at,
            return_to,
        })
    }

    /// What the confirmation screen shows; consumes nothing.
    pub async fn link_details(&self, code: &str, cookie: Option<&str>) -> SsoResult<LinkProposal> {
        let payload = self
            .handoffs
            .peek(
                &digest(code),
                &digest(cookie.unwrap_or("")),
                self.clock.now(),
            )
            .await?
            .ok_or(SsoError::Refused(SsoRefusal::HandoffUnknown))?;
        match serde_json::from_str(&payload) {
            Ok(HandoffPayload::Link {
                target,
                key,
                email,
                email_verified,
                proposal,
                ..
            }) => Ok(LinkProposal {
                provider: key.provider,
                issuer: key.issuer,
                subject: key.subject,
                email,
                email_verified,
                proposal,
                target,
            }),
            _ => Err(SsoRefusal::HandoffUnknown.into()),
        }
    }

    /// A human's confirmation: the Bearer of the named account and the
    /// cookie of `begin_link`, or nothing is linked.
    pub async fn confirm_link(
        &self,
        actor: i64,
        code: &str,
        cookie: Option<&str>,
        meta: Meta<'_>,
    ) -> SsoResult<()> {
        let HandoffPayload::Link {
            user_id,
            key,
            email,
            ..
        } = self.consume(code, cookie, meta).await?
        else {
            return Err(SsoRefusal::HandoffUnknown.into());
        };
        let target = self
            .users
            .by_id(user_id)
            .await?
            .ok_or(SsoError::Refused(SsoRefusal::WrongUser))?;
        if actor != user_id {
            self.record(
                Some(&target),
                "sso.link.refused",
                Some(&key.subject),
                Some("wrong_user"),
                meta,
            )
            .await;
            return Err(SsoRefusal::WrongUser.into());
        }
        if can_admin(&target.role) || self.is_bootstrap(&target) {
            return Err(SsoRefusal::AdminNotLinkable.into());
        }
        match self
            .identities
            .attach(user_id, &key.key(), email.as_deref(), self.clock.now())
            .await
        {
            Ok(()) => {}
            Err(StoreError::Conflict) => return Err(SsoRefusal::AlreadyLinked.into()),
            Err(e) => return Err(e.into()),
        }
        self.record(Some(&target), "sso.link", Some(&key.subject), None, meta)
            .await;
        Ok(())
    }

    /// Revokes the session token presented; returns where to end the
    /// provider's session when it was an SSO one.
    pub async fn logout(&self, token_id: Option<&str>) -> SsoResult<Option<String>> {
        let Some(id) = token_id else {
            return Ok(None);
        };
        let Some(key) = self.identities.provenance(id).await? else {
            return Ok(None);
        };
        match self.tokens.delete(id).await {
            Ok(()) | Err(StoreError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
        let Ok(p) = self.provider(&key.authority.provider) else {
            return Ok(None);
        };
        Ok(p.end_session(&format!(
            "{}/login",
            self.settings.base_url.trim_end_matches('/')
        ))
        .await)
    }

    pub async fn unlink(&self, user_id: i64, key: &IdentityKey) -> SsoResult<()> {
        self.identities.detach(user_id, key).await?;
        Ok(())
    }

    /// Never the bootstrap account.
    pub async fn disable_user(&self, user: &User) -> SsoResult<()> {
        if self.is_bootstrap(user) {
            return Err(SsoRefusal::AdminNotLinkable.into());
        }
        self.identities
            .disable_user(user.id, DisabledBy::Admin, self.clock.now())
            .await?;
        Ok(())
    }

    pub async fn enable_user(&self, user: &User) -> SsoResult<()> {
        self.identities.enable_user(user.id).await?;
        Ok(())
    }

    pub async fn disable_link(&self, key: &IdentityKey) -> SsoResult<()> {
        self.identities.disable_link(key).await?;
        Ok(())
    }

    /// The server's own probe of every provider: the only input of the
    /// reauthentication clock's suspension.
    pub async fn probe_all(&self) {
        for p in &self.providers {
            let reachable = p.probe().await;
            let authority = &p.profile().authority;
            if let Err(e) = self
                .identities
                .record_probe(authority, reachable, self.clock.now())
                .await
            {
                tracing::warn!(error = %e, provider = %authority.provider, "failed to record an SSO probe");
            }
        }
    }

    /// Removes handoffs nobody claimed.
    pub async fn purge(&self) -> Result<u64, StoreError> {
        self.handoffs.purge_expired(self.clock.now()).await
    }
}

/// Startup, before the listener opens: every known authority is configured,
/// declared retired (revoked here) or declared migrated (moved here); any
/// other refuses the start and revokes nothing.
pub async fn reconcile_providers(
    identities: &dyn IdentityStore,
    declared: &identity::Declared<'_>,
) -> anyhow::Result<Vec<Reconcile>> {
    let known = identities.authorities().await?;
    let plan = identity::reconcile(&known, declared).map_err(|e| anyhow::anyhow!("{e}"))?;
    for step in &plan {
        match step {
            Reconcile::Revoke(a) => {
                let revoked = identities.revoke_authority(a).await?;
                tracing::warn!(provider = %a.provider, issuer = %a.issuer, revoked, "retired SSO provider: its credentials are revoked");
            }
            Reconcile::Migrate { from, to } => {
                identities.migrate_authority(from, to).await?;
                tracing::warn!(provider = %from.provider, from = %from.issuer, to = %to.issuer, "SSO issuer migrated");
            }
            Reconcile::Keep => {}
        }
    }
    Ok(plan)
}

#[cfg(test)]
#[path = "sso_tests.rs"]
mod tests;
