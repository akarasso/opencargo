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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use opencargo::domain::{CacheEntry, CacheEntryId, NewEntry, RepoId, Webhook};
use opencargo::error::StoreError;
use opencargo::ports::proxy_cache::ProxyCacheStore;
use opencargo::ports::webhooks::{NewWebhook, WebhookPatch, WebhookStore};

/// Which port a queued failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortId {
    Webhooks,
    ProxyCache,
}

#[derive(Default)]
struct State {
    webhooks: Vec<Webhook>,
    cache: Vec<CacheEntry>,
    next_id: i64,
    next_cache_id: i64,
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

    pub fn proxy_cache(&self) -> Arc<dyn ProxyCacheStore> {
        Arc::new(ProxyCache(self.0.clone()))
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

struct ProxyCache(Arc<Mutex<State>>);

impl ProxyCache {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut state = self.0.lock().unwrap();
        match state.refusal(PortId::ProxyCache) {
            Some(err) => Err(err),
            None => act(&mut state),
        }
    }

    fn at(state: &mut State, repo: RepoId, kind: &str, key: &str) -> Option<usize> {
        state
            .cache
            .iter()
            .position(|row| row.repository_id == repo && row.kind == kind && row.cache_key == key)
    }
}

#[async_trait]
impl ProxyCacheStore for ProxyCache {
    async fn entry(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheEntry>, StoreError> {
        self.with(|state| {
            let found = Self::at(state, repo, kind, key).map(|at| {
                let mut row = state.cache[at].clone();
                row.fresh = CacheEntry::fresh_at(row.expires_at, now);
                row
            });
            Ok(found)
        })
    }

    async fn upsert(&self, entry: &NewEntry<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.with(|state| {
            let at = Self::at(state, entry.repository_id, entry.kind, entry.cache_key);
            let id = match at {
                Some(at) => state.cache[at].id,
                None => {
                    state.next_cache_id += 1;
                    state.next_cache_id
                }
            };
            let stored = CacheEntry {
                id,
                repository_id: entry.repository_id,
                kind: entry.kind.to_string(),
                cache_key: entry.cache_key.to_string(),
                status: entry.status,
                storage_path: entry.storage_path.map(str::to_string),
                content_type: entry.content_type.map(str::to_string),
                etag: entry.etag.map(str::to_string),
                digest: entry.digest.map(str::to_string),
                size: entry.size,
                fetched_at: now,
                expires_at: CacheEntry::expiry(entry.ttl(), now),
                last_used_at: now,
                fresh: true,
            };
            match at {
                Some(at) => state.cache[at] = stored,
                None => state.cache.push(stored),
            }
            Ok(())
        })
    }

    async fn touch(
        &self,
        id: CacheEntryId,
        ttl: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            if let Some(row) = state.cache.iter_mut().find(|row| row.id == id) {
                row.last_used_at = now;
                if let Some(until) = CacheEntry::expiry(ttl, now) {
                    row.expires_at = Some(until);
                }
            }
            Ok(())
        })
    }

    async fn delete_for_repo(&self, repo: RepoId) -> Result<u64, StoreError> {
        self.with(|state| {
            let before = state.cache.len();
            state.cache.retain(|row| row.repository_id != repo);
            Ok((before - state.cache.len()) as u64)
        })
    }

    async fn evictable(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CacheEntry>, StoreError> {
        self.with(|state| {
            let unused_since = now - idle;
            let mut rows: Vec<CacheEntry> = state
                .cache
                .iter()
                .filter(|row| {
                    let expired = row.status != 200 && row.expires_at.is_some_and(|at| at <= now);
                    expired || row.last_used_at < unused_since
                })
                .cloned()
                .map(|mut row| {
                    row.fresh = CacheEntry::fresh_at(row.expires_at, now);
                    row
                })
                .collect();
            rows.sort_by_key(|row| row.id);
            rows.truncate(limit as usize);
            Ok(rows)
        })
    }

    async fn delete(&self, id: CacheEntryId) -> Result<(), StoreError> {
        self.with(|state| {
            state.cache.retain(|row| row.id != id);
            Ok(())
        })
    }
}
