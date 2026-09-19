pub mod age;
pub mod distance;
pub mod facts;
mod memo;
pub mod pacer;
pub mod rules;
pub mod startup;
mod totals;
mod writer;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::auth::middleware::AuthUser;
use crate::domain::{CacheRepo, Format, Repository};
use crate::error::StoreError;
use crate::ports::events::Events;
use crate::ports::policy::{PolicyStore, ReportFilter, Totals};
use crate::proxy::engine::Cached;
use crate::proxy::ProxyEngine;
use crate::registry::resolve::{Cx, Upstream};
use crate::ports::vulns::VulnFeed;

pub use age::Age;
use facts::NpmSlot;
use memo::Memo;
use pacer::Pacer;
use rules::osv_severity::OsvMemo;
use rules::{PolicyConfig, Rule};
use totals::TotalsCache;
use writer::Notify;

pub const QUEUE: usize = 4096;
pub const INFLIGHT: usize = 64;
pub const BATCH: usize = 64;
const NPM_MEMO: usize = 1024;
const WARN_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Token,
    User,
    Static,
    Anonymous,
}

impl ActorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ActorKind::Token => "token",
            ActorKind::User => "user",
            ActorKind::Static => "static",
            ActorKind::Anonymous => "anonymous",
        }
    }
}

/// `name` is display only; `user_id` is the identity.
#[derive(Debug, Clone, Serialize)]
pub struct Actor {
    pub name: String,
    pub kind: ActorKind,
    pub user_id: Option<i64>,
}

impl Actor {
    pub fn of(auth: Option<&AuthUser>) -> Self {
        match auth {
            Some(AuthUser {
                token_name: Some(name),
                user_id,
                ..
            }) => Self {
                name: name.clone(),
                kind: ActorKind::Token,
                user_id: *user_id,
            },
            Some(AuthUser {
                user_id: Some(id),
                username,
                ..
            }) => Self {
                name: username.clone(),
                kind: ActorKind::User,
                user_id: Some(*id),
            },
            Some(user) => Self {
                name: user.username.clone(),
                kind: ActorKind::Static,
                user_id: None,
            },
            None => Self {
                name: "anonymous".to_string(),
                kind: ActorKind::Anonymous,
                user_id: None,
            },
        }
    }
}

/// Values copied from the served `Cached`, never the handle; `Oci.served`
/// is the child row the writer attaches to a released index and
/// `Oci.parsed` the body the row is dated from, parsed once by the
/// writer's classification so the gather never reads the file again.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Source {
    Npm {
        filename: String,
        digest: Option<String>,
    },
    Cargo {
        cksum: String,
    },
    Go {
        digest: Option<String>,
    },
    /// The page the file was listed on dates it (PEP 700 `upload-time`).
    Pypi {
        digest: Option<String>,
        uploaded: Option<String>,
    },
    Nuget {
        body: Cached,
        published: Option<DateTime<Utc>>,
    },
    /// A path served through a raw proxy: nothing dates it.
    Raw {
        digest: Option<String>,
    },
    Oci {
        body: Cached,
        served: Option<Cached>,
        parsed: Option<serde_json::Value>,
    },
}

/// One served artifact on its way to the writer; `name` is the client's.
#[derive(Debug)]
pub struct Pending {
    pub requested_repo: String,
    pub member: Repository,
    pub upstream: Upstream,
    pub format: Format,
    pub name: String,
    pub version: Option<String>,
    pub actor: Actor,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub requested_repo: String,
    pub member_repo: String,
    pub format: Format,
    pub name: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub actor: Actor,
    pub published_at: Option<DateTime<Utc>>,
    pub facts: Facts,
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub install_scripts: Option<bool>,
    pub date_source: &'static str,
}

/// Timing knobs; tests shrink them, production runs the defaults.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub child_ttl: Duration,
    pub pacer_period: Duration,
    pub pacer_cooldown: Duration,
    pub gather_timeout: Duration,
    pub notify_period: Duration,
    pub refresh_floor: Duration,
    pub flush_period: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            child_ttl: Duration::from_secs(60),
            pacer_period: Duration::from_secs(1),
            pacer_cooldown: Duration::from_secs(60),
            gather_timeout: Duration::from_secs(15),
            notify_period: Duration::from_millis(500),
            refresh_floor: Duration::from_secs(300),
            flush_period: Duration::from_millis(100),
        }
    }
}

impl Tuning {
    /// The pacer's waiting-line cap: the waiters a lane can serve within
    /// one `gather_timeout`.
    pub fn pacer_waiters(&self) -> usize {
        let period = self.pacer_period.as_millis().max(1);
        ((self.gather_timeout.as_millis() / period) as usize).max(1)
    }
}

pub(crate) type ChildKey = (i64, String, String);

pub(crate) struct Shared {
    pub store: Arc<dyn PolicyStore>,
    pub proxy: ProxyEngine,
    pub rules: Vec<Box<dyn Rule>>,
    pub config: HashMap<String, PolicyConfig>,
    pub events: Arc<dyn Events>,
    pub scanner: Arc<dyn VulnFeed>,
    pub osv_memo: Arc<OsvMemo>,
    pub recent_children: Mutex<HashMap<ChildKey, VecDeque<(u64, Instant)>>>,
    pub parked: Mutex<HashMap<u64, (Pending, Instant)>>,
    pub seq: AtomicU64,
    pub npm_facts: Mutex<Memo<(i64, String), NpmSlot>>,
    pub notify: Mutex<Notify>,
    pub inflight: Arc<Semaphore>,
    pub cargo_pacer: Pacer,
    pub tuning: Tuning,
    pub dropped: AtomicU64,
    pub totals: TotalsCache,
    warned_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    pub npm_parses: AtomicU64,
}

impl Shared {
    pub fn config_for(&self, member: &str) -> PolicyConfig {
        self.config.get(member).cloned().unwrap_or_default()
    }

    /// The writer's tick has OCI work while an index is parked or a child
    /// key is alive.
    pub fn holds_oci_state(&self) -> bool {
        !self.parked.lock().unwrap().is_empty() || !self.recent_children.lock().unwrap().is_empty()
    }

    fn dropped(&self, why: &str) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::record_policy_dropped();
        let mut warned = self.warned_at.lock().unwrap();
        if warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
            *warned = Some(Instant::now());
            warn!(
                dropped = self.dropped.load(Ordering::Relaxed),
                "policy event dropped: {why}"
            );
        }
    }
}

#[derive(Clone)]
pub struct PolicyEngine {
    tx: mpsc::Sender<Pending>,
    shared: Arc<Shared>,
}

impl PolicyEngine {
    /// Builds the channel and spawns the writer; always called under a runtime.
    pub fn new(
        store: Arc<dyn PolicyStore>,
        config: &HashMap<String, PolicyConfig>,
        scanner: Arc<dyn VulnFeed>,
        events: Arc<dyn Events>,
        proxy: ProxyEngine,
    ) -> Self {
        Self::new_tuned(store, config, scanner, events, proxy, Tuning::default())
    }

    #[doc(hidden)]
    pub fn new_tuned(
        store: Arc<dyn PolicyStore>,
        config: &HashMap<String, PolicyConfig>,
        scanner: Arc<dyn VulnFeed>,
        events: Arc<dyn Events>,
        proxy: ProxyEngine,
        tuning: Tuning,
    ) -> Self {
        let (engine, writer) = Self::unspawned(store, config, scanner, events, proxy, tuning);
        tokio::spawn(writer);
        engine
    }

    /// The engine and its writer future, not spawned: unit tests drive it.
    pub(crate) fn unspawned(
        store: Arc<dyn PolicyStore>,
        config: &HashMap<String, PolicyConfig>,
        scanner: Arc<dyn VulnFeed>,
        events: Arc<dyn Events>,
        proxy: ProxyEngine,
        tuning: Tuning,
    ) -> (Self, impl Future<Output = ()>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let osv_memo = rules::osv_severity::new_memo();
        let shared = Arc::new(Shared {
            store,
            proxy,
            rules: rules::all_rules(scanner.clone(), osv_memo.clone()),
            config: config.clone(),
            events,
            scanner,
            osv_memo,
            recent_children: Mutex::new(HashMap::new()),
            parked: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            npm_facts: Mutex::new(Memo::new(NPM_MEMO)),
            notify: Mutex::new(Notify::default()),
            inflight: Arc::new(Semaphore::new(INFLIGHT)),
            cargo_pacer: Pacer::default(),
            tuning,
            dropped: AtomicU64::new(0),
            totals: TotalsCache::default(),
            warned_at: Mutex::new(None),
            #[cfg(test)]
            npm_parses: AtomicU64::new(0),
        });
        let writer = writer::run_writer(rx, shared.clone());
        (Self { tx, shared }, writer)
    }

    /// The leaves' gate: a borrow, no clone, before anything else happens.
    pub fn records(&self, member: &str) -> bool {
        self.shared
            .config
            .get(member)
            .is_some_and(|cfg| !cfg.is_empty())
    }

    pub fn config_for(&self, member: &str) -> PolicyConfig {
        self.shared.config_for(member)
    }

    /// Never awaits: a full queue drops and counts the event.
    pub fn record(&self, p: Pending) {
        match self.tx.try_send(p) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.shared.dropped("queue full"),
            Err(TrySendError::Closed(_)) => self.shared.dropped("writer gone"),
        }
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Report totals for `f`, snapshotted under `key` (the filter as the
    /// client spelled it) so refetches read only the rows landed since.
    pub async fn totals(
        &self,
        f: &ReportFilter<'_>,
        key: String,
    ) -> Result<Totals, StoreError> {
        self.shared
            .totals
            .totals(self.shared.store.as_ref(), f, key)
            .await
    }

    /// After an erasure: every snapshot counted rows that are gone.
    pub fn forget_totals(&self) {
        self.shared.totals.forget();
    }

    /// The strategies' names in report order, the report's `rule` filter domain.
    pub fn rule_names(&self) -> Vec<&'static str> {
        self.shared.rules.iter().map(|r| r.name()).collect()
    }

    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    #[cfg(test)]
    pub(crate) fn queue_capacity(&self) -> usize {
        self.tx.capacity()
    }
}

/// What the resolver needs of the policy report: whether this member is
/// watched at all, and somewhere to put what it resolved. Two methods, not a
/// `PolicyEngine`, so a leaf can be walked without a queue, a writer and a
/// database behind it.
pub trait ResolutionRecorder: Send + Sync {
    fn records(&self, member: &str) -> bool;
    fn record(&self, pending: Pending);
}

impl ResolutionRecorder for PolicyEngine {
    fn records(&self, member: &str) -> bool {
        PolicyEngine::records(self, member)
    }

    fn record(&self, pending: Pending) {
        PolicyEngine::record(self, pending)
    }
}

/// The one line each proxy leaf adds over its `Found(cached)`. Returns at
/// once unless the member records; only then clones member and upstream
/// and runs `source`.
pub fn record(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    format: Format,
    name: &str,
    version: Option<String>,
    source: impl FnOnce() -> Source,
) {
    if !cx.policy.records(&member.0.name) {
        return;
    }
    cx.policy.record(Pending {
        requested_repo: cx.url.0.to_string(),
        member: member.0.clone(),
        upstream: up.clone(),
        format,
        name: name.to_string(),
        version,
        actor: Actor::of(cx.auth),
        source: source(),
    });
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
