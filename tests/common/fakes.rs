//! In-memory doubles for the ports, shared by the lib's unit tests (through
//! `src/testing.rs`) and by every integration test that declares
//! `mod common;`. Everything here spells the crate `opencargo::`, like the
//! rest of `tests/common/`, which is what lets one file compile on both
//! sides.
//!
//! One state behind one `Arc`, not one fake per port: a cascade crosses
//! aggregates, and independent maps cannot represent one. Handles are cheap
//! views onto it.
//!
//! The `allow` is this file's own: `tests/common/mod.rs`'s inner attribute
//! does not reach it once the lib includes it under a different parent, and
//! CI runs clippy over the test targets with `-D warnings`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use opencargo::domain::{ApiToken, Rights, User, Webhook};
use opencargo::error::StoreError;
use opencargo::ports::permissions::{PermissionStore, RepoRights};
use opencargo::ports::tokens::{NewToken, TokenStore};
use opencargo::ports::users::{NewUser, UserPatch, UserStore};
use opencargo::ports::webhooks::{NewWebhook, WebhookPatch, WebhookStore};

/// Which port a queued failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortId {
    Webhooks,
    Users,
    Tokens,
    Permissions,
}

/// One grant, keyed the way the table is.
#[derive(Clone)]
struct Grant {
    user_id: i64,
    repository_id: i64,
    rights: Rights,
}

#[derive(Default)]
struct State {
    webhooks: Vec<Webhook>,
    users: Vec<User>,
    tokens: Vec<ApiToken>,
    grants: Vec<Grant>,
    /// Repository names, so a grant can be listed with the repository it is
    /// on; a grant whose repository is absent lists with `None`, which is the
    /// state the admin screen has to show.
    repo_names: HashMap<i64, String>,
    next_id: i64,
    /// Queued refusals: a fake that cannot fail only ever proves the happy
    /// path, which the integration suite already covers.
    failures: Vec<(PortId, StoreError)>,
}

impl State {
    fn refusal(&mut self, port: PortId) -> Option<StoreError> {
        let at = self
            .failures
            .iter()
            .position(|(queued, _)| *queued == port)?;
        Some(self.failures.remove(at).1)
    }
}

#[derive(Clone, Default)]
pub struct FakeDb(Arc<Mutex<State>>);

impl FakeDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn webhooks(&self) -> Arc<dyn WebhookStore> {
        Arc::new(Webhooks(self.0.clone()))
    }

    pub fn users(&self) -> Arc<dyn UserStore> {
        Arc::new(Users(self.0.clone()))
    }

    pub fn tokens(&self) -> Arc<dyn TokenStore> {
        Arc::new(Tokens(self.0.clone()))
    }

    pub fn perms(&self) -> Arc<dyn PermissionStore> {
        Arc::new(Permissions(self.0.clone()))
    }

    /// Name a repository so grants on it list with that name. The fake holds
    /// no repository aggregate of its own until `RepositoryStore` lands.
    pub fn name_repository(&self, id: i64, name: &str) {
        self.0
            .lock()
            .unwrap()
            .repo_names
            .insert(id, name.to_string());
    }

    /// Make the next call on `port` fail with `err`.
    pub fn fail_next(&self, port: PortId, err: StoreError) {
        self.0.lock().unwrap().failures.push((port, err));
    }
}

struct Webhooks(Arc<Mutex<State>>);

impl Webhooks {
    /// The fake's own stamp spelling, deliberately not the SQLite adapter's:
    /// a contract clause that only holds for one rendering is a clause about
    /// an adapter, not about the port.
    fn stamp(now: DateTime<Utc>) -> String {
        now.to_rfc3339()
    }

    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut state = self.0.lock().unwrap();
        match state.refusal(PortId::Webhooks) {
            Some(err) => Err(err),
            None => act(&mut state),
        }
    }

    fn find(state: &mut State, id: i64) -> Result<&mut Webhook, StoreError> {
        state
            .webhooks
            .iter_mut()
            .find(|hook| hook.id == id)
            .ok_or(StoreError::NotFound)
    }

    fn insert(state: &mut State, hook: &NewWebhook<'_>, now: DateTime<Utc>) -> Webhook {
        state.next_id += 1;
        let stored = Webhook {
            id: state.next_id,
            url: hook.url.to_string(),
            events: hook.events.clone(),
            secret: hook.secret.map(|secret| secret.to_string()),
            active: true,
            created_at: Self::stamp(now),
            updated_at: Self::stamp(now),
        };
        state.webhooks.push(stored.clone());
        stored
    }
}

#[async_trait]
impl WebhookStore for Webhooks {
    async fn all(&self) -> Result<Vec<Webhook>, StoreError> {
        self.with(|state| Ok(state.webhooks.clone()))
    }

    async fn by_id(&self, id: i64) -> Result<Option<Webhook>, StoreError> {
        self.with(|state| Ok(state.webhooks.iter().find(|hook| hook.id == id).cloned()))
    }

    async fn create(
        &self,
        hook: &NewWebhook<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        self.with(|state| Ok(Self::insert(state, hook, now)))
    }

    async fn update(
        &self,
        id: i64,
        patch: &WebhookPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        self.with(|state| {
            let stored = Self::find(state, id)?;
            if patch.touches_nothing() {
                return Ok(stored.clone());
            }
            if let Some(url) = patch.url {
                stored.url = url.to_string();
            }
            if let Some(events) = patch.events {
                stored.events = events.clone();
            }
            if let Some(secret) = patch.secret {
                stored.secret = secret.map(|secret| secret.to_string());
            }
            if let Some(active) = patch.active {
                stored.active = active;
            }
            stored.updated_at = Self::stamp(now);
            Ok(stored.clone())
        })
    }

    async fn delete(&self, id: i64) -> Result<(), StoreError> {
        self.with(|state| {
            let at = state
                .webhooks
                .iter()
                .position(|hook| hook.id == id)
                .ok_or(StoreError::NotFound)?;
            state.webhooks.remove(at);
            Ok(())
        })
    }

    async fn active(&self) -> Result<Vec<Webhook>, StoreError> {
        self.with(|state| {
            Ok(state
                .webhooks
                .iter()
                .filter(|hook| hook.active)
                .cloned()
                .collect())
        })
    }

    async fn ensure_seeded(
        &self,
        hooks: &[NewWebhook<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            if hooks.is_empty() || !state.webhooks.is_empty() {
                return Ok(());
            }
            for hook in hooks {
                Self::insert(state, hook, now);
            }
            Ok(())
        })
    }
}

/// The shared guard every handle takes: one queued refusal for this port, or
/// the action.
fn guarded<T>(
    state: &Arc<Mutex<State>>,
    port: PortId,
    act: impl FnOnce(&mut State) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let mut state = state.lock().unwrap();
    match state.refusal(port) {
        Some(err) => Err(err),
        None => act(&mut state),
    }
}

struct Users(Arc<Mutex<State>>);

impl Users {
    fn find<'s>(state: &'s mut State, username: &str) -> Result<&'s mut User, StoreError> {
        state
            .users
            .iter_mut()
            .find(|user| user.username == username)
            .ok_or(StoreError::NotFound)
    }
}

#[async_trait]
impl UserStore for Users {
    async fn by_name(&self, username: &str) -> Result<Option<User>, StoreError> {
        guarded(&self.0, PortId::Users, |state| {
            Ok(state
                .users
                .iter()
                .find(|user| user.username == username)
                .cloned())
        })
    }

    async fn by_id(&self, id: i64) -> Result<Option<User>, StoreError> {
        guarded(&self.0, PortId::Users, |state| {
            Ok(state.users.iter().find(|user| user.id == id).cloned())
        })
    }

    async fn all(&self) -> Result<Vec<User>, StoreError> {
        guarded(&self.0, PortId::Users, |state| Ok(state.users.clone()))
    }

    async fn create(&self, user: &NewUser<'_>, now: DateTime<Utc>) -> Result<User, StoreError> {
        guarded(&self.0, PortId::Users, |state| {
            if state.users.iter().any(|u| u.username == user.username) {
                return Err(StoreError::Conflict);
            }
            state.next_id += 1;
            let stored = User {
                id: state.next_id,
                username: user.username.to_string(),
                email: user.email.map(str::to_string),
                password_hash: user.password_hash.to_string(),
                role: user.role.to_string(),
                must_change_password: false,
                created_at: now,
                updated_at: now,
            };
            state.users.push(stored.clone());
            Ok(stored)
        })
    }

    async fn update(
        &self,
        username: &str,
        patch: &UserPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError> {
        guarded(&self.0, PortId::Users, |state| {
            let stored = Self::find(state, username)?;
            if patch.touches_nothing() {
                return Ok(stored.clone());
            }
            if let Some(email) = patch.email {
                stored.email = Some(email.to_string());
            }
            if let Some(hash) = patch.password_hash {
                stored.password_hash = hash.to_string();
            }
            if let Some(role) = patch.role {
                stored.role = role.to_string();
            }
            if let Some(must_change) = patch.must_change_password {
                stored.must_change_password = must_change;
            }
            stored.updated_at = now;
            Ok(stored.clone())
        })
    }

    async fn delete(&self, username: &str) -> Result<(), StoreError> {
        guarded(&self.0, PortId::Users, |state| {
            let at = state
                .users
                .iter()
                .position(|user| user.username == username)
                .ok_or(StoreError::NotFound)?;
            let gone = state.users.remove(at).id;
            state.tokens.retain(|token| token.user_id != gone);
            state.grants.retain(|grant| grant.user_id != gone);
            Ok(())
        })
    }
}

struct Tokens(Arc<Mutex<State>>);

#[async_trait]
impl TokenStore for Tokens {
    async fn by_prefix(&self, prefix: &str) -> Result<Option<ApiToken>, StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            Ok(state
                .tokens
                .iter()
                .find(|token| token.prefix == prefix)
                .cloned())
        })
    }

    async fn by_id(&self, id: &str) -> Result<Option<ApiToken>, StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            Ok(state.tokens.iter().find(|token| token.id == id).cloned())
        })
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<ApiToken>, StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            let mut mine: Vec<ApiToken> = state
                .tokens
                .iter()
                .filter(|token| token.user_id == user_id)
                .cloned()
                .collect();
            mine.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok(mine)
        })
    }

    async fn create(
        &self,
        token: &NewToken<'_>,
        now: DateTime<Utc>,
    ) -> Result<ApiToken, StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            if state.tokens.iter().any(|stored| stored.id == token.id) {
                return Err(StoreError::Conflict);
            }
            let stored = ApiToken {
                id: token.id.to_string(),
                user_id: token.user_id,
                name: token.name.to_string(),
                prefix: token.prefix.to_string(),
                token_hash: token.token_hash.to_string(),
                expires_at: token.expires_at,
                last_used_at: None,
                created_at: now,
            };
            state.tokens.push(stored.clone());
            Ok(stored)
        })
    }

    async fn delete(&self, id: &str) -> Result<(), StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            let at = state
                .tokens
                .iter()
                .position(|token| token.id == id)
                .ok_or(StoreError::NotFound)?;
            state.tokens.remove(at);
            Ok(())
        })
    }

    async fn touch(&self, id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        guarded(&self.0, PortId::Tokens, |state| {
            let stored = state
                .tokens
                .iter_mut()
                .find(|token| token.id == id)
                .ok_or(StoreError::NotFound)?;
            stored.last_used_at = Some(now);
            Ok(())
        })
    }
}

struct Permissions(Arc<Mutex<State>>);

#[async_trait]
impl PermissionStore for Permissions {
    async fn rights(
        &self,
        user_id: i64,
        repository_id: i64,
    ) -> Result<Option<Rights>, StoreError> {
        guarded(&self.0, PortId::Permissions, |state| {
            Ok(state
                .grants
                .iter()
                .find(|grant| grant.user_id == user_id && grant.repository_id == repository_id)
                .map(|grant| grant.rights))
        })
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<RepoRights>, StoreError> {
        guarded(&self.0, PortId::Permissions, |state| {
            Ok(state
                .grants
                .iter()
                .filter(|grant| grant.user_id == user_id)
                .map(|grant| RepoRights {
                    repository_id: grant.repository_id,
                    repository: state.repo_names.get(&grant.repository_id).cloned(),
                    rights: grant.rights,
                })
                .collect())
        })
    }

    async fn set(
        &self,
        user_id: i64,
        repository_id: i64,
        rights: Rights,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        guarded(&self.0, PortId::Permissions, |state| {
            match state
                .grants
                .iter_mut()
                .find(|grant| grant.user_id == user_id && grant.repository_id == repository_id)
            {
                Some(grant) => grant.rights = rights,
                None => state.grants.push(Grant {
                    user_id,
                    repository_id,
                    rights,
                }),
            }
            Ok(())
        })
    }

    async fn revoke(&self, user_id: i64, repository_id: i64) -> Result<(), StoreError> {
        guarded(&self.0, PortId::Permissions, |state| {
            state
                .grants
                .retain(|grant| !(grant.user_id == user_id && grant.repository_id == repository_id));
            Ok(())
        })
    }
}
