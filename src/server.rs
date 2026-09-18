pub mod rewrite;

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::adapters::sqlite::SqliteStores;
use crate::app::events::Announce;
use crate::app::place::Placer;
use crate::app::promote::PromoteVersion;
use crate::app::publish::PublishVersion;
use crate::app::publish_tail::{PublishGate, PublishTail};
use crate::app::reclaim::{ReclaimOrphans, ReclaimPolicy};
use crate::ports::reclaim::ReclaimStore;
use crate::ports::referenced::ReferencedKeys;
use crate::app::authenticate::{Authenticate, AuthenticateDeps, OpenGate, Refusal};
use crate::auth::middleware::{auth_middleware, AuthState};
use crate::ports::secrets::ServerSecretStore;
use crate::ports::signing::RegistryTokenSigner;
use crate::auth::rate_limit::RateLimiter;
use crate::config::{Config, RepositoryConfig, WebhookConfig};
use crate::domain::Subscription;
use crate::policy::PolicyEngine;
use crate::ports::audit::AuditStore;
use crate::ports::clock::Clock;
use crate::ports::dashboard::DashboardRead;
use crate::ports::deps::DependencyStore;
use crate::ports::events::Events;
use crate::ports::ids::Ids;
use crate::ports::oci::OciStore;
use crate::ports::packages::PackageStore;
use crate::ports::permissions::PermissionStore;
use crate::ports::policy::PolicyStore;
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::ports::repositories::RepositoryStore;
use crate::ports::search::SearchIndex;
use crate::ports::tokens::{NewToken, TokenStore};
use crate::ports::users::{NewUser, UserPatch, UserStore};
use crate::ports::webhooks::{NewWebhook, WebhookStore};
use crate::proxy::{ProxyEngine, Timeouts, TtlConfig, UpstreamAuth, UpstreamCreds};
use crate::storage::{StorageBackend, StoreIdentity};
use crate::ports::vulns::{VulnFeed, VulnStore};
use crate::telemetry;
use crate::telemetry::vulns::VulnScanner;
use crate::telemetry::webhooks::WebhookDispatcher;

/// Maximum request body size accepted by the server (1 GiB).
///
/// cargo crates and OCI blobs/layers routinely exceed axum's 2 MiB default,
/// which previously made cargo publish and OCI pushes fail with 413. The
/// cargo/OCI handlers use the `Bytes` extractor (subject to this limit);
/// npm/go read their body via `to_bytes(..)` with their own explicit caps and
/// are unaffected. Bodies are still buffered fully in memory today, so this is
/// a deliberate hard cap rather than `DefaultBodyLimit::disable()` — switching
/// storage to streaming (P1) is the follow-up that lets this grow safely.
const MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<dyn StorageBackend>,
    pub storage_ready: Arc<StorageReadiness>,
    /// What the proxy remembers; the engine holds it too, and the background
    /// sweep needs it without going through the engine.
    pub cache: Arc<dyn ProxyCacheStore>,
    pub auth: Arc<AuthState>,
    pub proxy: ProxyEngine,
    /// Per-repository upstream credentials, keyed by name; a missing key is the default.
    pub upstream_auth: Arc<HashMap<String, UpstreamCreds>>,
    pub base_url: String,
    pub metrics_handle: PrometheusHandle,
    pub registry_tokens: Arc<dyn RegistryTokenSigner>,
    pub publish_rate_limiter: Arc<RateLimiter>,
    pub token_rate_limiter: Arc<RateLimiter>,
    pub webhook_dispatcher: Arc<WebhookDispatcher>,
    pub webhooks: Arc<dyn WebhookStore>,
    pub users: Arc<dyn UserStore>,
    pub tokens: Arc<dyn TokenStore>,
    pub permissions: Arc<dyn PermissionStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub search: Arc<dyn SearchIndex>,
    pub oci: Arc<dyn OciStore>,
    pub reclaim: Arc<dyn ReclaimStore>,
    pub referenced: Arc<dyn ReferencedKeys>,
    pub audit: Arc<dyn AuditStore>,
    pub deps: Arc<dyn DependencyStore>,
    pub vulns: Arc<dyn VulnStore>,
    /// The report's rows; the engine beside it owns the writing and the
    /// totals cache, and holds the same store.
    pub policy_store: Arc<dyn PolicyStore>,
    /// The web UI's read model: one method per panel, and the only reader of
    /// the joins no store owns.
    pub dashboard: Arc<dyn DashboardRead>,
    pub vuln_scanner: Arc<dyn VulnFeed>,
    pub vuln_scan_config: crate::config::VulnScanConfig,
    /// Real-time event bus feeding the `/api/v1/events/ws` WebSocket.
    pub events: Arc<dyn Events>,
    /// The wall clock, for the two callers that cannot be handed a `now`.
    pub clock: Arc<dyn Clock>,
    /// The generated identifiers that reach a client.
    pub ids: Arc<dyn Ids>,
    /// Records proxy resolutions for the policy report; off until a rule is on.
    pub policy: PolicyEngine,
}

impl AppState {
    /// The gate every publish passes before its first write.
    pub fn publish_gate(&self) -> PublishGate {
        PublishGate::new(self.vuln_scanner.clone(), self.vuln_scan_config.clone())
    }

    /// The tail every publish runs once its version is serveable: the
    /// webhook, the event and its audience, then the scan.
    pub fn publish_tail(&self) -> PublishTail {
        PublishTail::new(
            self.announce(),
            self.webhook_dispatcher.clone(),
            self.vuln_scanner.clone(),
            self.vulns.clone(),
        )
    }

    /// The only placer of shared keys.
    pub fn placer(&self) -> Arc<Placer> {
        Arc::new(Placer::new(self.reclaim.clone(), self.storage.clone()))
    }

    pub fn publish_version(&self) -> PublishVersion {
        PublishVersion::new(self.packages.clone(), self.repos.clone(), self.placer())
    }

    pub fn promote_version(&self) -> PromoteVersion {
        PromoteVersion::new(
            self.packages.clone(),
            self.repos.clone(),
            self.storage.clone(),
            self.placer(),
        )
    }

    /// The only deleter of shared keys, over this state's stores.
    pub fn reclaim_orphans(&self) -> ReclaimOrphans {
        ReclaimOrphans::new(
            self.reclaim.clone(),
            self.referenced.clone(),
            self.storage.clone(),
            ReclaimPolicy::default(),
        )
    }

    /// The audience decision, for the one caller outside a publish: a
    /// promotion announces the same way.
    pub fn announce(&self) -> Announce {
        Announce::new(self.events.clone(), self.repos.clone())
    }
}

/// Migrate a database and nothing else: the `opencargo migrate` subcommand, so
/// the binary reaches the adapter through the composition root rather than
/// importing it.
pub async fn run_migrations(config: &Config) -> anyhow::Result<()> {
    let db = crate::adapters::sqlite::connect(&config.database.url).await?;
    crate::adapters::sqlite::migrate::run_all(&db).await?;
    Ok(())
}

/// A database URL turned into ports: the pool, migrated, with every store
/// over it and the stored names checked. The other half of the composition
/// root, so `main.rs` and the fixtures never name the adapter themselves.
pub async fn connect_stores(config: &Config) -> anyhow::Result<SqliteStores> {
    let db = crate::adapters::sqlite::connect(&config.database.url).await?;
    crate::adapters::sqlite::migrate::run_all(&db).await?;
    let stores = SqliteStores::new(db);
    crate::app::repo_spec::check_repository_names(stores.repositories().as_ref()).await?;
    Ok(stores)
}

/// A migrated database of its own, with every store over it: what the
/// temp-database fixtures open, so they reach the adapter through the
/// composition root instead of naming a pool themselves.
pub async fn open_stores(path: &std::path::Path) -> anyhow::Result<SqliteStores> {
    SqliteStores::open(path).await
}

/// What a store is built for; its identity defaults to the role's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Artifacts,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Artifacts => "artifacts",
        }
    }
}

/// Where a built store really points, for disjointness checks only: no
/// `Display`, no `Serialize`, and a `Debug` that names nothing.
pub struct ResolvedLocation {
    backend: &'static str,
    root: std::path::PathBuf,
}

impl std::fmt::Debug for ResolvedLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedLocation(..)")
    }
}

impl ResolvedLocation {
    /// Two locations share nothing: another backend, or roots neither of
    /// which contains the other.
    pub fn disjoint(&self, other: &ResolvedLocation) -> bool {
        self.backend != other.backend
            || !(self.root.starts_with(&other.root) || other.root.starts_with(&self.root))
    }
}

pub struct BuiltStorage {
    pub backend: Arc<dyn StorageBackend>,
    pub identity: StoreIdentity,
    pub location: ResolvedLocation,
}

pub fn build_storage(root: impl Into<std::path::PathBuf>, role: Role) -> BuiltStorage {
    let identity = StoreIdentity(role.name().to_string());
    #[allow(clippy::disallowed_types)]
    let fs = crate::adapters::fs::FilesystemStorage::new(root, identity.clone());
    let location = ResolvedLocation {
        backend: "fs",
        root: fs.root().to_path_buf(),
    };
    BuiltStorage {
        backend: Arc::new(fs),
        identity,
        location,
    }
}

/// A filesystem store under `root`, for the fixtures that may not name an
/// adapter.
pub fn filesystem(root: impl Into<std::path::PathBuf>) -> Arc<dyn StorageBackend> {
    build_storage(root, Role::Artifacts).backend
}

/// The real-time bus, for the same reason: `src/policy/`'s fixtures need one
/// and may not name an adapter.
pub fn event_bus() -> Arc<dyn Events> {
    Arc::new(crate::adapters::events::BroadcastEvents::new())
}

pub async fn build_state(config: &Config) -> anyhow::Result<AppState> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let policy_notes = crate::policy::startup::startup_notes(config).map_err(anyhow::Error::msg)?;
    ensure_directories(config)?;
    let stores = connect_stores(config).await?;
    let repos = stores.repositories();
    seed_repositories(repos.as_ref(), &config.repositories, Utc::now()).await?;

    let storage = build_storage(&config.server.storage_path, Role::Artifacts).backend;

    let (users, tokens, permissions) = (stores.users(), stores.tokens(), stores.permissions());

    let secrets: Arc<dyn ServerSecretStore> =
        Arc::new(crate::adapters::system::ProcessSecrets::default());
    let registry_tokens: Arc<dyn RegistryTokenSigner> = Arc::new(
        crate::registry::oci::token::TokenSigner::from_store(secrets.as_ref()).await?,
    );
    let auth = auth_state(config, &users, &tokens, &registry_tokens);

    ensure_admin_user(users.as_ref(), config).await?;

    let cache = stores.proxy_cache();
    let proxy = proxy_engine(config, storage.clone(), cache.clone(), &stores);
    let upstream_auth = Arc::new(upstream_creds(repos.as_ref(), config).await?);

    let metrics_handle = telemetry::init_metrics();

    let webhooks = stores.webhooks();
    seed_webhooks(webhooks.as_ref(), &config.webhooks, Utc::now()).await?;

    let webhook_dispatcher = Arc::new(WebhookDispatcher::new(webhooks.clone()));

    let vuln_scanner: Arc<dyn VulnFeed> = Arc::new(VulnScanner::new(&config.vuln_scan)?);

    let events = event_bus();
    let policy_store = stores.policy();
    let policy = PolicyEngine::new(
        policy_store.clone(),
        &config.policy,
        vuln_scanner.clone(),
        events.clone(),
        proxy.clone(),
    );
    report_ready(config, &policy_notes);

    Ok(AppState {
        storage_ready: Arc::new(StorageReadiness::new(storage.clone())),
        storage,
        cache,
        auth,
        proxy,
        upstream_auth,
        base_url: config.server.base_url.clone(),
        metrics_handle,
        registry_tokens,
        publish_rate_limiter: Arc::new(RateLimiter::new(30, 60)),
        token_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
        webhook_dispatcher,
        webhooks,
        users,
        tokens,
        permissions,
        repos,
        packages: stores.packages(),
        search: stores.search(),
        oci: stores.oci(),
        reclaim: stores.reclaim(),
        referenced: stores.referenced(),
        audit: stores.audit(),
        deps: stores.dependencies(),
        vulns: stores.vulns(),
        policy_store,
        dashboard: stores.dashboard(),
        vuln_scanner,
        vuln_scan_config: config.vuln_scan.clone(),
        events,
        clock: Arc::new(crate::adapters::system::SystemClock),
        ids: Arc::new(crate::adapters::system::UuidIds),
        policy,
    })
}

/// The one `Authenticate` and what the protocol adapters declare about
/// their routes.
fn auth_state(
    config: &Config,
    users: &Arc<dyn UserStore>,
    tokens: &Arc<dyn TokenStore>,
    signer: &Arc<dyn RegistryTokenSigner>,
) -> Arc<AuthState> {
    let authenticate = Authenticate::new(AuthenticateDeps {
        static_tokens: config.auth.static_tokens.clone(),
        token_prefix: config.auth.token_prefix.clone(),
        users: users.clone(),
        tokens: tokens.clone(),
        signer: signer.clone(),
        login_limiter: Arc::new(RateLimiter::new(5, 60)),
        token_limiter: Arc::new(RateLimiter::new(30, 60)),
        gate: Arc::new(OpenGate),
        clock: Arc::new(crate::adapters::system::SystemClock),
    });
    Arc::new(AuthState {
        anonymous_read: config.auth.anonymous_read,
        authenticate: Arc::new(authenticate),
        routes: vec![
            Arc::new(crate::registry::oci::auth_rules::OciRouteRules {
                base_url: config.server.base_url.clone(),
            }),
            Arc::new(crate::registry::cargo::auth_rules::CargoRouteRules),
        ],
        trusted_proxies: config.auth.trusted_proxies.clone(),
    })
}

/// The two directories the server writes into: the blob store, and the one
/// holding a SQLite file when that is where the database lives.
fn ensure_directories(config: &Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.server.storage_path)?;
    if let Some(path) = config.database.url.strip_prefix("sqlite:") {
        let path = path.split('?').next().unwrap_or(path);
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// The proxy engine over its two ports, with the configured timeouts.
fn proxy_engine(
    config: &Config,
    storage: Arc<dyn StorageBackend>,
    cache: Arc<dyn ProxyCacheStore>,
    stores: &SqliteStores,
) -> ProxyEngine {
    let ttl = TtlConfig {
        default_secs: parse_duration_secs(&config.proxy.default_ttl),
        negative_secs: parse_duration_secs(&config.proxy.negative_cache_ttl),
    };
    let connect_timeout_secs = parse_duration_secs(&config.proxy.connect_timeout);
    ProxyEngine::new(
        storage,
        cache,
        stores.repositories(),
        stores.reclaim(),
        Timeouts::from_connect_secs(connect_timeout_secs),
        ttl,
    )
}

/// Create the configured admin account on a fresh install, and warn on every
/// later boot while its generated password is still the one on disk.
async fn ensure_admin_user(users: &dyn UserStore, config: &Config) -> anyhow::Result<()> {
    let admin_username = &config.auth.admin.username;
    if admin_username.is_empty() {
        return Ok(());
    }
    let password_file = {
        let storage = std::path::Path::new(&config.server.storage_path);
        let data_dir = storage.parent().unwrap_or(std::path::Path::new("data"));
        data_dir.join("admin.password")
    };

    if let Some(user) = users.by_name(admin_username).await? {
        if password_file.exists() && user.must_change_password {
            warn!(
                "Admin password has not been changed yet. Initial password is still in {}",
                password_file.display()
            );
        }
        return Ok(());
    }

    let from_env = std::env::var("OPENCARGO_ADMIN_PASSWORD").is_ok();
    let raw_password = initial_admin_password(config);
    let password_hash = crate::auth::users::hash_password(&raw_password)
        .map_err(|e| anyhow::anyhow!("failed to hash admin password: {e}"))?;
    let admin = users
        .create(
            &NewUser {
                username: admin_username,
                email: None,
                password_hash: &password_hash,
                role: "admin",
            },
            Utc::now(),
        )
        .await?;

    if from_env {
        // Password from a k8s Secret: no file to leave behind, no forced change.
        info!(username = %admin_username, "Admin user created with password from OPENCARGO_ADMIN_PASSWORD");
    } else {
        users
            .update(
                &admin.username,
                &UserPatch {
                    must_change_password: Some(true),
                    ..UserPatch::default()
                },
                Utc::now(),
            )
            .await?;
        if let Some(parent) = password_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&password_file, &raw_password)?;
        warn!(
            "Initial admin password written to {} — change it on first login",
            password_file.display()
        );
    }
    info!(username = %admin_username, "Initial admin user created");
    Ok(())
}

/// Env var, then a config value that is not one of the two placeholders, then
/// a random one. Setting the variable at all means the config file's value is
/// not wanted, so an empty one falls through to random rather than to it.
fn initial_admin_password(config: &Config) -> String {
    if let Ok(env_pw) = std::env::var("OPENCARGO_ADMIN_PASSWORD") {
        if !env_pw.is_empty() {
            return env_pw;
        }
    } else if !config.auth.admin.password.is_empty()
        && config.auth.admin.password != "admin"
        && config.auth.admin.password != "changeme"
    {
        return config.auth.admin.password.clone();
    }
    crate::auth::users::generate_random_password()
}

/// The configured repositories, validated as an API write would validate
/// them and then seeded: the config file starts a deployment, it does not own
/// it afterwards.
///
/// The whole list is `pending`, so a group may name a member declared further
/// down the file.
async fn seed_repositories(
    store: &dyn RepositoryStore,
    configured: &[RepositoryConfig],
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let empty: Vec<String> = Vec::new();
    let pending: Vec<crate::domain::Pending<'_>> = configured
        .iter()
        .map(|repo| crate::domain::Pending {
            name: &repo.name,
            kind: repo.repo_type,
            format: repo.format,
            members: repo.members.as_deref().unwrap_or(&empty),
        })
        .collect();
    let specs: Vec<crate::domain::RepoSpec<'_>> = configured
        .iter()
        .map(|repo| crate::domain::RepoSpec {
            name: &repo.name,
            kind: repo.repo_type,
            format: repo.format,
            visibility: repo.visibility,
            upstream: repo.upstream.as_deref(),
            members: repo.members.as_deref().unwrap_or(&empty),
        })
        .collect();
    // One at a time, validated against what is already stored: a group that
    // names a member seeded earlier in this same pass must see its row, or a
    // mutual membership would validate as two pending entries and be seeded.
    for spec in &specs {
        crate::app::repo_spec::validate_spec(store, spec, &pending)
            .await
            .map_err(|e| anyhow::anyhow!("repository {}: {e}", spec.name))?;
        store.ensure_seeded(std::slice::from_ref(spec), now).await?;
    }
    info!("Repository seeding complete ({} configured)", specs.len());
    Ok(())
}

/// The configured registrations, in the port's vocabulary: the config file
/// seeds a deployment and the store owns them afterwards.
async fn seed_webhooks(
    store: &dyn WebhookStore,
    configured: &[WebhookConfig],
    now: DateTime<Utc>,
) -> Result<(), crate::error::StoreError> {
    let events: Vec<Subscription> = configured
        .iter()
        .map(|hook| Subscription::of_names(&hook.events))
        .collect();
    let hooks: Vec<NewWebhook<'_>> = configured
        .iter()
        .zip(&events)
        .map(|(hook, events)| NewWebhook {
            url: &hook.url,
            events,
            secret: hook.secret.as_deref(),
        })
        .collect();
    store.ensure_seeded(&hooks, now).await
}

/// Recording is personal data: say which members do it, at startup.
/// What booted, and every configuration note worth a warning about it.
fn report_ready(config: &Config, notes: &crate::policy::startup::StartupNotes) {
    info!(
        storage_path = %config.server.storage_path,
        base_url = %config.server.base_url,
        repos = config.repositories.len(),
        "Application state initialized"
    );
    warn_policy_notes(notes);
}

fn warn_policy_notes(notes: &crate::policy::startup::StartupNotes) {
    if !notes.recording.is_empty() {
        warn!(
            members = ?notes.recording,
            "policy rules enabled: every download through these members records its actor, artifact and time"
        );
    }
    if !notes.unknown.is_empty() {
        warn!(keys = ?notes.unknown, "[policy.*] keys naming no configured repository");
    }
    if !notes.inapplicable.is_empty() {
        warn!(rules = ?notes.inapplicable, "policy rules that can only answer not_applicable on an OCI member");
    }
}

pub fn build_router(state: AppState) -> Router {
    let auth_state = state.auth.clone();
    let metrics_handle = state.metrics_handle.clone();

    // Metrics endpoint served on a separate nested router (no auth required)
    let metrics_routes = Router::new()
        .route("/metrics", get(telemetry::metrics_endpoint))
        .with_state(metrics_handle);

    // Admin API routes
    let api_routes = Router::new()
        .route("/api/v1/users", get(crate::api::users::list_users).post(crate::api::users::create_user))
        .route(
            "/api/v1/users/{username}",
            get(crate::api::users::get_user)
                .put(crate::api::users::update_user)
                .delete(crate::api::users::delete_user),
        )
        .route(
            "/api/v1/users/{username}/password",
            put(crate::api::users::change_password),
        )
        .route(
            "/api/v1/users/{username}/tokens",
            get(crate::api::tokens::list_tokens).post(crate::api::tokens::create_token),
        )
        .route(
            "/api/v1/users/{username}/tokens/{token_id}",
            delete(crate::api::tokens::delete_token),
        )
        // Repository CRUD (admin) — GET also serves dashboard list (anonymous read allowed via auth middleware)
        .route(
            "/api/v1/repositories",
            get(crate::api::dashboard::list_repositories)
                .post(crate::api::repositories::create_repository),
        )
        .route(
            "/api/v1/repositories/{name}",
            get(crate::api::repositories::get_repository)
                .put(crate::api::repositories::update_repository)
                .delete(crate::api::repositories::delete_repository),
        )
        .route(
            "/api/v1/repositories/{name}/purge-cache",
            post(crate::api::repositories::purge_cache),
        )
        // Permissions (admin)
        .route(
            "/api/v1/users/{username}/permissions",
            get(crate::api::permissions::list_permissions),
        )
        .route(
            "/api/v1/users/{username}/permissions/{repo_name}",
            put(crate::api::permissions::set_permission)
                .delete(crate::api::permissions::delete_permission),
        )
        // Webhooks CRUD (admin)
        .route(
            "/api/v1/webhooks",
            get(crate::api::webhooks::list_webhooks)
                .post(crate::api::webhooks::create_webhook),
        )
        .route(
            "/api/v1/webhooks/{id}",
            put(crate::api::webhooks::update_webhook)
                .delete(crate::api::webhooks::delete_webhook),
        )
        .route(
            "/api/v1/webhooks/{id}/test",
            post(crate::api::webhooks::test_webhook),
        )
        .route("/api/v1/system/audit", get(crate::api::audit::list_audit))
        .route(
            "/api/v1/policy/report",
            get(crate::api::policy::report).delete(crate::api::policy::erase),
        )
        .route("/api/v1/policy/rules", get(crate::api::policy::rules))
        .route("/api/v1/me/policy", get(crate::api::policy::me_policy))
        // Promote routes — scoped packages (@scope/name)
        .route(
            "/api/v1/promote/@{scope}/{name}/{version}",
            post(crate::api::promote::promote_package),
        )
        .route(
            "/api/v1/promotions/@{scope}/{name}/{version}",
            get(crate::api::promote::list_promotions),
        )
        // Promote routes — unscoped packages
        .route(
            "/api/v1/promote/{name}/{version}",
            post(crate::api::promote::promote_package_unscoped),
        )
        .route(
            "/api/v1/promotions/{name}/{version}",
            get(crate::api::promote::list_promotions_unscoped),
        )
        // Impact analysis routes (auth required) — scoped
        .route(
            "/api/v1/deps/@{scope}/{name}/versions/{version}/impact",
            get(crate::api::deps::impact_analysis),
        )
        // Impact analysis routes (auth required) — unscoped
        .route(
            "/api/v1/deps/{name}/versions/{version}/impact",
            get(crate::api::deps::impact_analysis_unscoped),
        )
        // Vulnerability scan routes (auth required)
        .route(
            "/api/v1/vulns/@{scope}/{name}/{version}",
            get(crate::api::vulns::get_vulns),
        )
        .route(
            "/api/v1/vulns/@{scope}/{name}/{version}/rescan",
            post(crate::api::vulns::rescan),
        )
        .route(
            "/api/v1/vulns/{name}/{version}",
            get(crate::api::vulns::get_vulns_unscoped),
        )
        .route(
            "/api/v1/vulns/{name}/{version}/rescan",
            post(crate::api::vulns::rescan_unscoped),
        );

    // Dashboard / frontend API routes. These run INSIDE the auth middleware
    // (merged before the auth layer below) so the handlers can filter out
    // private repositories for non-admin/anonymous callers.
    let dashboard_routes = Router::new()
        .route("/api/v1/dashboard", get(crate::api::dashboard::dashboard_stats))
        .route("/api/v1/me/permissions", get(crate::api::me::my_permissions))
        .route("/api/v1/packages", get(crate::api::dashboard::list_packages))
        .route("/api/v1/packages/{*path}", get(crate::api::dashboard::package_detail))
        .route("/api/v1/search", get(crate::api::dashboard::search))
        // Dependency graph routes
        .route("/api/v1/deps/@{scope}/{name}/dependencies", get(crate::api::deps::get_dependencies))
        .route("/api/v1/deps/@{scope}/{name}/dependents", get(crate::api::deps::get_dependents))
        .route("/api/v1/deps/{name}/dependencies", get(crate::api::deps::get_dependencies_unscoped))
        .route("/api/v1/deps/{name}/dependents", get(crate::api::deps::get_dependents_unscoped));

    // Web UI routes (no auth required)
    let web_routes = crate::web::web_routes().with_state(state.clone());

    // npm login route — must be outside the auth middleware because the
    // caller authenticates with username/password in the request body, not
    // with a Bearer token.
    let npm_login_route = Router::new()
        .route("/-/user/org.couchdb.user:{username}", put(npm_login))
        .with_state(state.clone());

    // Real-time WebSocket — outside the auth middleware because browsers
    // cannot set an Authorization header on WebSocket handshakes; the client
    // authenticates with its first frame instead (see api::ws).
    let ws_route = Router::new()
        .route("/api/v1/events/ws", get(crate::api::ws::ws_handler))
        .with_state(state.clone());

    // OCI token endpoint — outside the auth middleware, it checks Basic
    // credentials itself and issues anonymous tokens.
    let oci_token_route = crate::registry::oci::routes::token_routes().with_state(state.clone());

    let router = Router::new()
        // Health checks (no auth)
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        // Whoami (needs auth)
        .route("/-/whoami", get(whoami))
        // Admin API routes
        .merge(api_routes)
        .merge(crate::registry::npm::routes::routes())
        .merge(crate::registry::cargo::routes::routes())
        .merge(crate::registry::go::routes::routes())
        .merge(crate::registry::oci::routes::routes())
        // Dashboard / frontend API + dependency graph — INSIDE the auth layer
        // so handlers receive the optional AuthUser and filter private repos.
        .merge(dashboard_routes)
        // Auth middleware
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .with_state(state)
        // npm login (outside auth middleware — uses password auth)
        .merge(npm_login_route)
        // Real-time WebSocket (outside auth middleware — first-frame auth)
        .merge(ws_route)
        .merge(oci_token_route)
        // Web UI (outside auth middleware)
        .merge(web_routes)
        // Metrics endpoint (outside auth middleware)
        .merge(metrics_routes)
        // Raise the body-size limit for the whole app so cargo crates and OCI
        // blobs/layers (which use the `Bytes` extractor) are not rejected at
        // axum's 2 MiB default. See MAX_BODY_BYTES.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // HTTP metrics middleware (outermost layer, records all requests)
        .layer(axum::middleware::from_fn(
            telemetry::http_metrics_middleware,
        ))
        // Security response headers on every response (defense in depth).
        .layer(axum::middleware::from_fn(security_headers_middleware));

    // The pre-route rewrites must run BEFORE route matching, and
    // `Router::layer` runs after it, so the finished router is wrapped in a
    // `map_request` service and re-exposed as the fallback of a fresh
    // route-less Router. This keeps the return type (`Router`) so both
    // `main.rs` and the integration tests get the rewrite for free.
    Router::new().fallback_service(tower::Layer::layer(
        &tower::util::MapRequestLayer::new(rewrite::pre_route),
        router,
    ))
}

/// Add hardening response headers to every response.
///
/// The CSP allows same-origin assets plus inline styles (the embedded SPA uses
/// them); tighten it further if the frontend stops needing `'unsafe-inline'`.
async fn security_headers_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, HeaderValue};
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self' data:; connect-src 'self'; \
             object-src 'none'; base-uri 'self'; frame-ancestors 'none'",
        ),
    );
    response
}

async fn health_live() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn health_ready(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    // Reachability, asked of a port rather than of a pool: a repository
    // nobody can be called is one index lookup answering `None`, and a store
    // that cannot answer it is a store the server is not ready to serve from.
    // Wider than the `SELECT 1` this replaced, on purpose: a pool that is up
    // over a database with no `repositories` table is not ready either, and
    // used to report itself healthy.
    if state.repos.by_name("").await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "reason": "database"})),
        );
    }
    if !state.storage_ready.ready().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "reason": "storage"})),
        );
    }
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// The storage probe behind `/health/ready`, memoized briefly so a probe
/// storm never becomes a storage storm.
pub struct StorageReadiness {
    storage: Arc<dyn StorageBackend>,
    last: tokio::sync::Mutex<Option<(std::time::Instant, bool)>>,
}

impl StorageReadiness {
    const MEMO: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage,
            last: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn ready(&self) -> bool {
        let mut last = self.last.lock().await;
        if let Some((at, ok)) = *last {
            if at.elapsed() < Self::MEMO {
                return ok;
            }
        }
        let ok = self.storage.probe().await.is_ok();
        *last = Some((std::time::Instant::now(), ok));
        ok
    }
}

async fn whoami(
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    if let Some(user) = request
        .extensions()
        .get::<crate::auth::middleware::AuthUser>()
    {
        Json(json!({
            "username": user.username,
            "role": user.role,
            "must_change_password": user.must_change_password,
        }))
    } else {
        Json(json!({"username": "anonymous", "role": "anonymous", "must_change_password": false}))
    }
}

// ---------------------------------------------------------------------------
// npm login compatibility
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NpmLoginBody {
    name: String,
    password: String,
}

/// PUT /-/user/org.couchdb.user:{username} — npm login
///
/// Receives `{ "name": "username", "password": "password" }` and returns
/// `{ "ok": true, "token": "trg_xxx" }` after verifying credentials.
async fn npm_login(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(_username): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let login: NpmLoginBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid request body"})),
            )
                .into_response();
        }
    };

    let user = match state.auth.authenticate.password(&login.name, &login.password).await {
        Ok(user) => user,
        Err(Refusal::Throttled) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "too many login attempts, try again later"})),
            )
                .into_response();
        }
        Err(Refusal::Unavailable) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "authentication temporarily unavailable, try again"})),
            )
                .into_response();
        }
        Err(Refusal::Invalid) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid credentials"})),
            )
                .into_response();
        }
    };
    let Some(user_id) = user.user_id else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    };

    let must_change = user.must_change_password;

    let Ok(raw_token) = issue_login_token(&state, user_id).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create token"})),
        )
            .into_response();
    };

    (StatusCode::CREATED, Json(json!({"ok": true, "token": raw_token, "must_change_password": must_change}))).into_response()
}

/// The session token an `npm login` walks away with: 30 days by default, like
/// every other credential this endpoint has ever issued.
async fn issue_login_token(
    state: &AppState,
    user_id: i64,
) -> Result<String, crate::error::StoreError> {
    let (raw_token, token_hash) = crate::auth::tokens::generate_token("trg_");
    let now = state.clock.now();
    state
        .tokens
        .create(
            &NewToken {
                id: &uuid::Uuid::new_v4().to_string(),
                user_id,
                name: "npm-login",
                prefix: &raw_token[..16],
                token_hash: &token_hash,
                expires_at: Some(now + chrono::Duration::days(30)),
            },
            now,
        )
        .await?;
    Ok(raw_token)
}

const ENV_UPSTREAM_AUTH: &str = "OPENCARGO_UPSTREAM_AUTH_";
const ENV_DL_ALLOW_PRIVATE: &str = "OPENCARGO_DL_ALLOW_PRIVATE_";

/// `<REPO>` in `OPENCARGO_*_<REPO>`: the name uppercased, every byte outside
/// `[A-Za-z0-9]` becoming `_`.
pub(crate) fn env_repo_key(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The credentials of every repository row, seeded or API-created.
async fn upstream_creds(
    repos: &dyn RepositoryStore,
    config: &Config,
) -> anyhow::Result<HashMap<String, UpstreamCreds>> {
    let known: Vec<String> = repos.all().await?.into_iter().map(|r| r.name).collect();
    load_upstream_creds(&config.repositories, &known, std::env::vars())
}

/// Config credentials per repository, then the environment on top (env
/// wins). `known` lists every repository row, so a proxy created through
/// the API takes its `OPENCARGO_UPSTREAM_AUTH_<REPO>` at the next start.
fn load_upstream_creds(
    repos: &[RepositoryConfig],
    known: &[String],
    env: impl IntoIterator<Item = (String, String)>,
) -> anyhow::Result<HashMap<String, UpstreamCreds>> {
    let mut by_key: HashMap<String, &str> = HashMap::new();
    for name in repos.iter().map(|r| r.name.as_str()).chain(known.iter().map(String::as_str)) {
        let key = env_repo_key(name);
        match by_key.insert(key.clone(), name) {
            Some(other) if other != name => anyhow::bail!(
                "repositories '{other}' and '{name}' both map to the environment key '{key}'"
            ),
            _ => {}
        }
    }
    let mut creds = HashMap::new();
    for repo in repos {
        let token_realms = repo
            .token_realms
            .iter()
            .map(|r| {
                reqwest::Url::parse(r).map_err(|e| {
                    anyhow::anyhow!("repository '{}': invalid token realm '{r}': {e}", repo.name)
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        creds.insert(
            repo.name.clone(),
            UpstreamCreds {
                auth: repo.upstream_auth.clone(),
                token_realms,
                dl_allow_private: repo.dl_allow_private,
            },
        );
    }
    for (var, value) in env {
        let (prefix, key) = match (
            var.strip_prefix(ENV_UPSTREAM_AUTH),
            var.strip_prefix(ENV_DL_ALLOW_PRIVATE),
        ) {
            (Some(key), _) => (ENV_UPSTREAM_AUTH, key),
            (None, Some(key)) => (ENV_DL_ALLOW_PRIVATE, key),
            (None, None) => continue,
        };
        let Some(name) = by_key.get(key) else {
            warn!(var = %var, "environment override names no known repository");
            continue;
        };
        let entry = creds.entry(name.to_string()).or_default();
        if prefix == ENV_UPSTREAM_AUTH {
            entry.auth =
                Some(UpstreamAuth::parse_env(&value).map_err(|e| anyhow::anyhow!("{var}: {e}"))?);
        } else {
            entry.dl_allow_private = matches!(value.trim(), "1" | "true" | "yes");
        }
    }
    Ok(creds)
}

/// Parse a duration string like "10s", "24h", "30m" into seconds.
/// Falls back to 10 seconds on parse failure.
fn parse_duration_secs(s: &str) -> u64 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.parse().unwrap_or(10)
    } else if let Some(n) = s.strip_suffix('m') {
        n.parse::<u64>().unwrap_or(10) * 60
    } else if let Some(n) = s.strip_suffix('h') {
        n.parse::<u64>().unwrap_or(10) * 3600
    } else {
        s.parse().unwrap_or(10)
    }
}

/// Decode percent-encoded slashes (`%2f` / `%2F`) in request URIs.
///
/// npm/pnpm clients send scoped package names with encoded slashes
/// (e.g. `@scope%2fname`).  This function rewrites the URI in-place so
/// that axum's router can match `/{repo}/@{scope}/{name}` patterns.
///
/// Must be applied as a `tower` `map_request` layer **outside** the
/// axum `Router` — using `Router::layer()` would run _after_ route
/// matching and therefore have no effect on 404s.
pub fn decode_percent_encoded_slashes<B>(
    mut req: axum::http::Request<B>,
) -> axum::http::Request<B> {
    let path = req.uri().path();
    if path.contains("%2f") || path.contains("%2F") {
        let decoded = path.replace("%2f", "/").replace("%2F", "/");
        let new_uri_str = match req.uri().query() {
            Some(q) => format!("{}?{}", decoded, q),
            None => decoded,
        };
        // The decoded form can stop being a parseable URI (e.g. `/%2f` decodes
        // to `//`, an authority form with an empty authority). This middleware
        // runs on every request, so it must never panic: keep the original URI
        // and let the router 404 it.
        match new_uri_str.parse() {
            Ok(new_uri) => *req.uri_mut() = new_uri,
            Err(e) => {
                tracing::warn!(uri = %req.uri(), error = %e, "re-parse of percent-decoded URI failed; keeping original");
            }
        }
    }
    req
}

#[cfg(test)]
mod tests {
    use super::{decode_percent_encoded_slashes, env_repo_key, load_upstream_creds};
    use crate::config::RepositoryConfig;
    use crate::proxy::UpstreamAuth;

    fn proxy(name: &str) -> RepositoryConfig {
        RepositoryConfig {
            name: name.to_string(),
            repo_type: crate::domain::RepoKind::Proxy,
            upstream: Some("https://registry.npmjs.org".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn env_repo_key_mangles_and_refuses_collisions() {
        assert_eq!(env_repo_key("npm-proxy"), "NPM_PROXY");
        assert_eq!(env_repo_key("npm.proxy"), "NPM_PROXY");
        assert_eq!(env_repo_key("Oci_Hub2"), "OCI_HUB2");

        let err = load_upstream_creds(&[proxy("npm-proxy"), proxy("npm.proxy")], &[], Vec::new())
            .expect_err("two names mangling alike must refuse startup");
        assert!(err.to_string().contains("npm-proxy") && err.to_string().contains("npm.proxy"));
        let known = ["npm-proxy".to_string(), "npm.proxy".to_string()];
        assert!(
            load_upstream_creds(&[proxy("npm-proxy")], &known, Vec::new()).is_err(),
            "a row created through the API collides the same way"
        );

        let mut configured = proxy("npm-proxy");
        configured.upstream_auth = Some(UpstreamAuth::Bearer {
            token: "from-config".to_string(),
        });
        configured.token_realms = vec!["https://auth.example/token".to_string()];
        let env = vec![
            (
                "OPENCARGO_UPSTREAM_AUTH_NPM_PROXY".to_string(),
                "basic:u:p".to_string(),
            ),
            (
                "OPENCARGO_DL_ALLOW_PRIVATE_NPM_PROXY".to_string(),
                "1".to_string(),
            ),
            (
                "OPENCARGO_UPSTREAM_AUTH_UNKNOWN".to_string(),
                "bearer:x".to_string(),
            ),
            ("OPENCARGO_BASE_URL".to_string(), "http://x".to_string()),
        ];
        let known = ["npm-proxy".to_string(), "other".to_string(), "api-made".to_string()];
        let env = env
            .into_iter()
            .chain([(
                "OPENCARGO_UPSTREAM_AUTH_API_MADE".to_string(),
                "bearer:api-token".to_string(),
            )])
            .collect::<Vec<_>>();
        let creds = load_upstream_creds(&[configured, proxy("other")], &known, env).unwrap();
        assert!(
            matches!(&creds["api-made"].auth, Some(UpstreamAuth::Bearer { token }) if token == "api-token"),
            "a repository that exists only in the database takes its env credentials"
        );
        assert!(!creds.contains_key("unknown"));
        let npm = &creds["npm-proxy"];
        assert!(
            matches!(&npm.auth, Some(UpstreamAuth::Basic { username, .. }) if username == "u"),
            "env wins over config"
        );
        assert!(npm.dl_allow_private);
        assert_eq!(npm.token_realms[0].as_str(), "https://auth.example/token");
        let other = &creds["other"];
        assert!(other.auth.is_none() && !other.dl_allow_private && other.token_realms.is_empty());

        assert!(load_upstream_creds(
            &[proxy("npm-proxy")],
            &[],
            vec![(
                "OPENCARGO_UPSTREAM_AUTH_NPM_PROXY".to_string(),
                "digest:x".to_string()
            )]
        )
        .is_err());
    }

    fn req(uri: &str) -> axum::http::Request<()> {
        axum::http::Request::builder()
            .uri(uri)
            .body(())
            .expect("test URI should build")
    }

    /// Nominal npm/pnpm case: scoped package names arrive with `%2f`-encoded
    /// slashes and must be decoded so the router can match `@{scope}/{name}`.
    #[test]
    fn decodes_scoped_npm_slashes() {
        let out = decode_percent_encoded_slashes(req("/npm-dev/@scope%2fpkg"));
        assert_eq!(out.uri().path(), "/npm-dev/@scope/pkg");

        // Uppercase encoding, with a query string that must survive.
        let out = decode_percent_encoded_slashes(req("/npm-dev/@scope%2Fpkg?write=true"));
        assert_eq!(out.uri().path(), "/npm-dev/@scope/pkg");
        assert_eq!(out.uri().query(), Some("write=true"));
    }

    /// A URI without any encoded slash passes through untouched.
    #[test]
    fn leaves_uris_without_encoded_slash_untouched() {
        let out = decode_percent_encoded_slashes(req("/npm-dev/react?abbrev=true"));
        assert_eq!(out.uri(), &"/npm-dev/react?abbrev=true".parse::<axum::http::Uri>().unwrap());
    }

    /// Degenerate inputs: none of these may panic (the old `.expect()` made
    /// this middleware a remote panic vector on every request). Today's
    /// `http::Uri` happens to re-parse all of these decoded forms; the
    /// invariant under test is that hostile paths flow through — decoded or
    /// left as-is — without crashing.
    #[test]
    fn degenerate_encoded_paths_never_panic() {
        for uri in ["/%2f", "/%2F%2f%2F", "/a%2f..%2f..%2fetc", "/%2f%2f?q=1"] {
            let out = decode_percent_encoded_slashes(req(uri));
            assert!(!out.uri().to_string().is_empty(), "uri {uri} must survive");
        }

        // Only the path is decoded: a `%2f` in the query must stay encoded.
        let out = decode_percent_encoded_slashes(req("/npm-dev/@a%2fb?rev=x%2fy"));
        assert_eq!(out.uri().path(), "/npm-dev/@a/b");
        assert_eq!(out.uri().query(), Some("rev=x%2fy"));
    }
}
