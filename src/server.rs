use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, head, post, put},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::auth::middleware::{auth_middleware, AuthState};
use crate::auth::rate_limit::RateLimiter;
use crate::config::Config;
use crate::proxy::ProxyClient;
use crate::storage::FilesystemStorage;
use crate::telemetry;
use crate::telemetry::vulns::VulnScanner;
use crate::telemetry::webhooks::WebhookDispatcher;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub storage: Arc<FilesystemStorage>,
    pub auth: Arc<AuthState>,
    pub proxy_client: ProxyClient,
    pub base_url: String,
    pub metrics_handle: PrometheusHandle,
    pub login_rate_limiter: Arc<RateLimiter>,
    pub publish_rate_limiter: Arc<RateLimiter>,
    pub token_rate_limiter: Arc<RateLimiter>,
    pub webhook_dispatcher: Arc<WebhookDispatcher>,
    pub vuln_scanner: Arc<VulnScanner>,
    pub vuln_scan_config: crate::config::VulnScanConfig,
}

pub async fn build_state(config: &Config) -> anyhow::Result<AppState> {
    // Ensure storage directory exists
    std::fs::create_dir_all(&config.server.storage_path)?;

    // Ensure database directory exists
    if let Some(path) = config.database.url.strip_prefix("sqlite:") {
        let path = path.split('?').next().unwrap_or(path);
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let db = crate::db::connect(&config.database.url).await?;
    crate::db::migrate(&db).await?;
    crate::db::init_repositories(&db, &config.repositories).await?;

    let storage = Arc::new(FilesystemStorage::new(&config.server.storage_path));

    let auth = Arc::new(AuthState {
        static_tokens: config.auth.static_tokens.clone(),
        anonymous_read: config.auth.anonymous_read,
        db: db.clone(),
    });

    // Create admin user from config if it doesn't exist
    if !config.auth.admin.username.is_empty() {
        let admin_username = &config.auth.admin.username;

        // Determine the password file path
        let password_file = {
            let storage = std::path::Path::new(&config.server.storage_path);
            let data_dir = storage.parent().unwrap_or(std::path::Path::new("data"));
            data_dir.join("admin.password")
        };

        match crate::db::get_user_by_username(&db, admin_username).await? {
            None => {
                // Admin user does not exist yet — create it
                // Priority: env var > config > random
                let raw_password = if let Ok(env_pw) = std::env::var("OPENCARGO_ADMIN_PASSWORD") {
                    if !env_pw.is_empty() { env_pw } else { crate::auth::users::generate_random_password() }
                } else if !config.auth.admin.password.is_empty()
                    && config.auth.admin.password != "admin"
                    && config.auth.admin.password != "changeme"
                {
                    config.auth.admin.password.clone()
                } else {
                    crate::auth::users::generate_random_password()
                };

                let from_env = std::env::var("OPENCARGO_ADMIN_PASSWORD").is_ok();

                let password_hash = crate::auth::users::hash_password(&raw_password)
                    .map_err(|e| anyhow::anyhow!("failed to hash admin password: {e}"))?;

                crate::db::create_user(&db, admin_username, None, &password_hash, "admin").await?;

                if from_env {
                    // Password from env var (k8s Secret) — no file, no forced change
                    info!(username = %admin_username, "Admin user created with password from OPENCARGO_ADMIN_PASSWORD");
                } else {
                    // Generated password — write to file and force change
                    if let Some(user) = crate::db::get_user_by_username(&db, admin_username).await? {
                        crate::db::set_must_change_password(&db, user.id, true).await?;
                    }
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
            }
            Some(user) => {
                // Admin user already exists
                if password_file.exists() && user.must_change_password == 1 {
                    warn!(
                        "Admin password has not been changed yet. Initial password is still in {}",
                        password_file.display()
                    );
                }
            }
        }
    }

    let connect_timeout_secs = parse_duration_secs(&config.proxy.connect_timeout);
    let proxy_client = ProxyClient::new(storage.clone(), db.clone(), connect_timeout_secs);

    // Initialize Prometheus metrics
    let metrics_handle = telemetry::init_metrics();

    // Seed webhooks from config into DB (only if no webhooks exist yet)
    crate::db::seed_webhooks(&db, &config.webhooks).await?;

    // Initialize webhook dispatcher (DB-backed)
    let webhook_dispatcher = Arc::new(WebhookDispatcher::new(db.clone()));

    // Initialize vulnerability scanner
    let vuln_scanner = Arc::new(VulnScanner::new(config.vuln_scan.enabled));

    info!(
        storage_path = %config.server.storage_path,
        base_url = %config.server.base_url,
        repos = config.repositories.len(),
        "Application state initialized"
    );

    Ok(AppState {
        db,
        storage,
        auth,
        proxy_client,
        base_url: config.server.base_url.clone(),
        metrics_handle,
        login_rate_limiter: Arc::new(RateLimiter::new(5, 60)),
        publish_rate_limiter: Arc::new(RateLimiter::new(30, 60)),
        token_rate_limiter: Arc::new(RateLimiter::new(10, 60)),
        webhook_dispatcher,
        vuln_scanner,
        vuln_scan_config: config.vuln_scan.clone(),
    })
}

pub fn build_router(state: AppState) -> Router {
    let auth_state = state.auth.clone();
    let metrics_handle = state.metrics_handle.clone();

    let npm_routes = Router::new()
        // Scoped packages: @scope/name
        .route(
            "/{repo}/@{scope}/{name}",
            get(crate::registry::npm::get_package)
                .put(crate::registry::npm::publish_package),
        )
        .route(
            "/{repo}/@{scope}/{name}/-/{filename}",
            get(crate::registry::npm::download_tarball),
        )
        // Unscoped packages
        .route(
            "/{repo}/{name}/-/{filename}",
            get(crate::registry::npm::download_tarball),
        )
        // Search
        .route(
            "/{repo}/-/v1/search",
            get(crate::registry::npm::search),
        )
        // Dist-tags for scoped packages
        .route(
            "/{repo}/-/package/@{scope}/{name}/dist-tags",
            get(crate::registry::npm::get_dist_tags),
        )
        .route(
            "/{repo}/-/package/@{scope}/{name}/dist-tags/{tag}",
            put(crate::registry::npm::put_dist_tag)
                .delete(crate::registry::npm::delete_dist_tag),
        );

    let cargo_routes = Router::new()
        // Cargo sparse registry index
        .route(
            "/{repo}/index/config.json",
            get(crate::registry::cargo::config_json),
        )
        .route(
            "/{repo}/index/1/{name}",
            get(crate::registry::cargo::get_index_entry),
        )
        .route(
            "/{repo}/index/2/{name}",
            get(crate::registry::cargo::get_index_entry),
        )
        .route(
            "/{repo}/index/3/{first}/{name}",
            get(crate::registry::cargo::get_index_entry),
        )
        .route(
            "/{repo}/index/{first_two}/{next_two}/{name}",
            get(crate::registry::cargo::get_index_entry),
        )
        // Cargo API
        .route(
            "/{repo}/api/v1/crates/new",
            put(crate::registry::cargo::publish_crate),
        )
        .route(
            "/{repo}/api/v1/crates/{name}/{version}/download",
            get(crate::registry::cargo::download_crate),
        )
        .route(
            "/{repo}/api/v1/crates/{name}/{version}/yank",
            delete(crate::registry::cargo::yank),
        )
        .route(
            "/{repo}/api/v1/crates/{name}/{version}/unyank",
            put(crate::registry::cargo::unyank),
        );

    let go_routes = Router::new()
        .route(
            "/{repo}/{module}/@v/list",
            get(crate::registry::go::list_versions),
        )
        .route(
            "/{repo}/{module}/@v/{version}",
            get(go_version_dispatch).put(crate::registry::go::publish_module),
        );

    // OCI / Docker container registry routes
    let oci_routes = Router::new()
        .route("/v2/", get(crate::registry::oci::api_version_check))
        .route(
            "/v2/{repo}/{name}/blobs/{digest}",
            head(crate::registry::oci::head_blob)
                .get(crate::registry::oci::get_blob)
                .delete(crate::registry::oci::delete_blob),
        )
        .route(
            "/v2/{repo}/{name}/blobs/uploads/",
            post(crate::registry::oci::start_upload),
        )
        .route(
            "/v2/{repo}/{name}/blobs/uploads/{uuid}",
            put(crate::registry::oci::complete_upload)
                .patch(crate::registry::oci::upload_chunk),
        )
        .route(
            "/v2/{repo}/{name}/manifests/{reference}",
            get(crate::registry::oci::get_manifest)
                .head(crate::registry::oci::head_manifest)
                .put(crate::registry::oci::put_manifest)
                .delete(crate::registry::oci::delete_manifest),
        )
        .route(
            "/v2/{repo}/{name}/tags/list",
            get(crate::registry::oci::list_tags),
        );

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

    // Dashboard / frontend API routes (no auth required)
    let dashboard_routes = Router::new()
        .route("/api/v1/dashboard", get(crate::api::dashboard::dashboard_stats))
        .route("/api/v1/packages", get(crate::api::dashboard::list_packages))
        .route("/api/v1/packages/{*path}", get(crate::api::dashboard::package_detail))
        .route("/api/v1/search", get(crate::api::dashboard::search))
        // Dependency graph routes (public, no auth)
        .route("/api/v1/deps/@{scope}/{name}/dependencies", get(crate::api::deps::get_dependencies))
        .route("/api/v1/deps/@{scope}/{name}/dependents", get(crate::api::deps::get_dependents))
        .route("/api/v1/deps/{name}/dependencies", get(crate::api::deps::get_dependencies_unscoped))
        .route("/api/v1/deps/{name}/dependents", get(crate::api::deps::get_dependents_unscoped))
        .with_state(state.clone());

    // Web UI routes (no auth required)
    let web_routes = crate::web::web_routes().with_state(state.clone());

    // npm login route — must be outside the auth middleware because the
    // caller authenticates with username/password in the request body, not
    // with a Bearer token.
    let npm_login_route = Router::new()
        .route("/-/user/org.couchdb.user:{username}", put(npm_login))
        .with_state(state.clone());

    Router::new()
        // Health checks (no auth)
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        // Whoami (needs auth)
        .route("/-/whoami", get(whoami))
        // Admin API routes
        .merge(api_routes)
        // npm protocol routes
        .merge(npm_routes)
        // Cargo sparse registry routes
        .merge(cargo_routes)
        // Go module registry routes
        .merge(go_routes)
        // OCI container registry routes
        .merge(oci_routes)
        // Auth middleware
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_middleware,
        ))
        .with_state(state)
        // npm login (outside auth middleware — uses password auth)
        .merge(npm_login_route)
        // Dashboard API (outside auth middleware)
        .merge(dashboard_routes)
        // Web UI (outside auth middleware)
        .merge(web_routes)
        // Metrics endpoint (outside auth middleware)
        .merge(metrics_routes)
        // HTTP metrics middleware (outermost layer, records all requests)
        .layer(axum::middleware::from_fn(
            telemetry::http_metrics_middleware,
        ))
}

/// Dispatch Go module version requests based on file extension.
///
/// The GOPROXY protocol uses URL suffixes like `.info`, `.mod`, `.zip` to
/// distinguish the type of response. We route them all through a single
/// `/{repo}/{module}/@v/{version}` pattern and dispatch here.
async fn go_version_dispatch(
    state: axum::extract::State<AppState>,
    path: Path<HashMap<String, String>>,
) -> crate::error::AppResult<axum::response::Response> {
    let version = path.get("version").cloned().unwrap_or_default();
    if version.ends_with(".info") {
        Ok(crate::registry::go::version_info(state, path).await?.into_response())
    } else if version.ends_with(".mod") {
        Ok(crate::registry::go::get_mod(state, path).await?.into_response())
    } else if version.ends_with(".zip") {
        Ok(crate::registry::go::get_zip(state, path).await?.into_response())
    } else {
        Err(crate::error::AppError::BadRequest(
            "unknown version file extension; expected .info, .mod, or .zip".to_string(),
        ))
    }
}

async fn health_live() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn health_ready(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    // Check DB connectivity
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "reason": "database"})),
        ),
    }
}

async fn whoami(
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    if let Some(user) = request
        .extensions()
        .get::<crate::auth::middleware::AuthUser>()
    {
        Json(json!({"username": user.username}))
    } else {
        Json(json!({"username": "anonymous"}))
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
    // Rate limit: 5 attempts per minute per username
    let rate_key = format!("npm_login:{}", _username);
    if !state.login_rate_limiter.check(&rate_key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "too many login attempts, try again later"})),
        )
            .into_response();
    }

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

    // Look up the user
    let user = match crate::db::get_user_by_username(&state.db, &login.name).await {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid credentials"})),
            )
                .into_response();
        }
    };

    // Verify password
    let password_ok = match crate::auth::users::verify_password(&login.password, &user.password_hash)
    {
        Ok(true) => true,
        _ => false,
    };

    if !password_ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    // Block login if password change is required
    if user.must_change_password == 1 {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Password change required. Log in to the web UI to change your password."})),
        )
            .into_response();
    }

    // Create a new API token for this login session
    let token_id = uuid::Uuid::new_v4().to_string();
    let (raw_token, token_hash) = crate::auth::tokens::generate_token("trg_");
    let prefix = &raw_token[..16];

    // Token expires in 30 days by default for npm login
    let expires_at = {
        let expiry = chrono::Utc::now() + chrono::Duration::days(30);
        expiry.format("%Y-%m-%d %H:%M:%S").to_string()
    };

    if let Err(_) = crate::db::create_api_token(
        &state.db,
        &token_id,
        user.id,
        "npm-login",
        prefix,
        &token_hash,
        Some(&expires_at),
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create token"})),
        )
            .into_response();
    }

    (StatusCode::CREATED, Json(json!({"ok": true, "token": raw_token}))).into_response()
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
pub fn decode_percent_encoded_slashes(
    mut req: axum::http::Request<axum::body::Body>,
) -> axum::http::Request<axum::body::Body> {
    let path = req.uri().path();
    if path.contains("%2f") || path.contains("%2F") {
        let decoded = path.replace("%2f", "/").replace("%2F", "/");
        let new_uri_str = match req.uri().query() {
            Some(q) => format!("{}?{}", decoded, q),
            None => decoded,
        };
        *req.uri_mut() = new_uri_str.parse().expect("decoded URI is invalid");
    }
    req
}
