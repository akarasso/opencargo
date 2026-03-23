use sqlx::SqlitePool;
use tracing::info;

// Re-export serde_json for convenience in this module
use serde_json;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub repo_type: String,
    pub format: String,
    pub visibility: String,
    pub upstream_url: Option<String>,
    pub config_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Package {
    pub id: i64,
    pub repository_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub license: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Version {
    pub id: i64,
    pub package_id: i64,
    pub version: String,
    pub metadata_json: String,
    pub checksum_sha1: Option<String>,
    pub checksum_sha256: Option<String>,
    pub integrity: Option<String>,
    pub size: i64,
    pub tarball_path: String,
    pub published_at: String,
    pub yanked: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub struct DistTag {
    pub id: i64,
    pub package_id: i64,
    pub tag: String,
    pub version_id: i64,
}

// ---------------------------------------------------------------------------
// Connection & migration
// ---------------------------------------------------------------------------

/// Create a connection pool and enable WAL mode.
pub async fn connect(url: &str) -> anyhow::Result<SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
    use std::str::FromStr;

    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    info!("Connected to SQLite database");
    Ok(pool)
}

/// Run the embedded SQL migrations.
pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let sql = include_str!("migrations/001_initial.sql");
    sqlx::raw_sql(sql).execute(pool).await?;

    let sql2 = include_str!("migrations/002_proxy_cache.sql");
    sqlx::raw_sql(sql2).execute(pool).await?;

    let sql3 = include_str!("migrations/003_auth.sql");
    sqlx::raw_sql(sql3).execute(pool).await?;

    let sql4 = include_str!("migrations/004_cargo.sql");
    // ALTER TABLE may fail if column already exists; ignore the error.
    let _ = sqlx::raw_sql(sql4).execute(pool).await;

    let sql5 = include_str!("migrations/005_must_change_password.sql");
    // ALTER TABLE may fail if column already exists; ignore the error.
    let _ = sqlx::raw_sql(sql5).execute(pool).await;

    let sql6 = include_str!("migrations/006_oci.sql");
    sqlx::raw_sql(sql6).execute(pool).await?;

    let sql7 = include_str!("migrations/007_fts5.sql");
    // FTS5 might not be available on all SQLite builds; ignore the error.
    let _ = sqlx::raw_sql(sql7).execute(pool).await;

    let sql8 = include_str!("migrations/008_deps.sql");
    sqlx::raw_sql(sql8).execute(pool).await?;

    let sql9 = include_str!("migrations/009_vulns.sql");
    sqlx::raw_sql(sql9).execute(pool).await?;

    let sql10 = include_str!("migrations/010_dynamic_config.sql");
    sqlx::raw_sql(sql10).execute(pool).await?;

    info!("Database migrations applied");
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository seeding
// ---------------------------------------------------------------------------

/// Insert pre-configured repositories if they do not already exist.
pub async fn init_repositories(
    pool: &SqlitePool,
    repos: &[crate::config::RepositoryConfig],
) -> anyhow::Result<()> {
    for repo in repos {
        let repo_type = match repo.repo_type {
            crate::config::RepositoryType::Hosted => "hosted",
            crate::config::RepositoryType::Proxy => "proxy",
            crate::config::RepositoryType::Group => "group",
        };

        let format = match repo.format {
            crate::config::RepositoryFormat::Npm => "npm",
            crate::config::RepositoryFormat::Cargo => "cargo",
            crate::config::RepositoryFormat::Oci => "oci",
            crate::config::RepositoryFormat::Go => "go",
            crate::config::RepositoryFormat::Pypi => "pypi",
        };

        let visibility = match repo.visibility {
            crate::config::Visibility::Public => "public",
            crate::config::Visibility::Private => "private",
        };

        let config_json = match repo.repo_type {
            crate::config::RepositoryType::Group => {
                let members = repo.members.clone().unwrap_or_default();
                Some(serde_json::json!({ "members": members }).to_string())
            }
            _ => None,
        };

        sqlx::query(
            "INSERT OR IGNORE INTO repositories (name, repo_type, format, visibility, upstream_url, config_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&repo.name)
        .bind(repo_type)
        .bind(format)
        .bind(visibility)
        .bind(repo.upstream.as_deref())
        .bind(config_json.as_deref())
        .execute(pool)
        .await?;
    }

    info!("Repository seeding complete ({} configured)", repos.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Query functions
// ---------------------------------------------------------------------------

pub async fn get_repository_by_name(
    pool: &SqlitePool,
    name: &str,
) -> Result<Option<Repository>, sqlx::Error> {
    sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE name = ?1")
        .bind(name)
        .fetch_optional(pool)
        .await
}

pub async fn get_package(
    pool: &SqlitePool,
    repo_id: i64,
    name: &str,
) -> Result<Option<Package>, sqlx::Error> {
    sqlx::query_as::<_, Package>(
        "SELECT * FROM packages WHERE repository_id = ?1 AND name = ?2",
    )
    .bind(repo_id)
    .bind(name)
    .fetch_optional(pool)
    .await
}

pub async fn create_package(
    pool: &SqlitePool,
    repo_id: i64,
    name: &str,
    description: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO packages (repository_id, name, description) VALUES (?1, ?2, ?3)",
    )
    .bind(repo_id)
    .bind(name)
    .bind(description)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn create_version(
    pool: &SqlitePool,
    package_id: i64,
    version: &str,
    metadata_json: &str,
    checksum_sha1: Option<&str>,
    checksum_sha256: Option<&str>,
    integrity: Option<&str>,
    size: i64,
    tarball_path: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO versions (package_id, version, metadata_json, checksum_sha1, checksum_sha256, integrity, size, tarball_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(package_id)
    .bind(version)
    .bind(metadata_json)
    .bind(checksum_sha1)
    .bind(checksum_sha256)
    .bind(integrity)
    .bind(size)
    .bind(tarball_path)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn get_versions(
    pool: &SqlitePool,
    package_id: i64,
) -> Result<Vec<Version>, sqlx::Error> {
    sqlx::query_as::<_, Version>("SELECT * FROM versions WHERE package_id = ?1 ORDER BY id")
        .bind(package_id)
        .fetch_all(pool)
        .await
}

pub async fn get_version(
    pool: &SqlitePool,
    package_id: i64,
    version: &str,
) -> Result<Option<Version>, sqlx::Error> {
    sqlx::query_as::<_, Version>(
        "SELECT * FROM versions WHERE package_id = ?1 AND version = ?2",
    )
    .bind(package_id)
    .bind(version)
    .fetch_optional(pool)
    .await
}

pub async fn set_dist_tag(
    pool: &SqlitePool,
    package_id: i64,
    tag: &str,
    version_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(package_id, tag) DO UPDATE SET version_id = excluded.version_id",
    )
    .bind(package_id)
    .bind(tag)
    .bind(version_id)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_dist_tags(
    pool: &SqlitePool,
    package_id: i64,
) -> Result<Vec<DistTag>, sqlx::Error> {
    sqlx::query_as::<_, DistTag>("SELECT * FROM dist_tags WHERE package_id = ?1")
        .bind(package_id)
        .fetch_all(pool)
        .await
}

pub async fn record_download(
    pool: &SqlitePool,
    version_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO downloads (version_id) VALUES (?1)")
        .bind(version_id)
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn set_yanked(
    pool: &SqlitePool,
    version_id: i64,
    yanked: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE versions SET yanked = ?1 WHERE id = ?2")
        .bind(if yanked { 1i64 } else { 0i64 })
        .bind(version_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Proxy cache helpers
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
pub struct ProxyCacheMeta {
    pub id: i64,
    pub repository_id: i64,
    pub cache_key: String,
    pub fetched_at: String,
    pub ttl_seconds: i64,
}

pub async fn upsert_proxy_cache_meta(
    pool: &SqlitePool,
    repository_id: i64,
    cache_key: &str,
    ttl_seconds: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO proxy_cache_meta (repository_id, cache_key, fetched_at, ttl_seconds)
         VALUES (?1, ?2, datetime('now'), ?3)
         ON CONFLICT(repository_id, cache_key)
         DO UPDATE SET fetched_at = datetime('now'), ttl_seconds = excluded.ttl_seconds",
    )
    .bind(repository_id)
    .bind(cache_key)
    .bind(ttl_seconds)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_proxy_cache_meta(
    pool: &SqlitePool,
    repository_id: i64,
    cache_key: &str,
) -> Result<Option<ProxyCacheMeta>, sqlx::Error> {
    sqlx::query_as::<_, ProxyCacheMeta>(
        "SELECT * FROM proxy_cache_meta WHERE repository_id = ?1 AND cache_key = ?2",
    )
    .bind(repository_id)
    .bind(cache_key)
    .fetch_optional(pool)
    .await
}

/// Check whether a cache entry is still fresh.
/// Returns `true` if the entry exists and `fetched_at + ttl_seconds > now`.
pub async fn is_proxy_cache_fresh(
    pool: &SqlitePool,
    repository_id: i64,
    cache_key: &str,
) -> bool {
    let result = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM proxy_cache_meta
         WHERE repository_id = ?1
           AND cache_key = ?2
           AND datetime(fetched_at, '+' || ttl_seconds || ' seconds') > datetime('now')",
    )
    .bind(repository_id)
    .bind(cache_key)
    .fetch_one(pool)
    .await;

    matches!(result, Ok(count) if count > 0)
}

/// Return the list of member repository names for a group repo.
pub fn parse_group_members(config_json: Option<&str>) -> Vec<String> {
    config_json
        .and_then(|json_str| serde_json::from_str::<serde_json::Value>(json_str).ok())
        .and_then(|v| v.get("members")?.as_array().cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Auth row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: String,
    pub must_change_password: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ApiToken {
    pub id: String,
    pub user_id: i64,
    pub name: String,
    pub prefix: String,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub permissions_json: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub repository: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub details_json: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// User CRUD
// ---------------------------------------------------------------------------

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    role: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, role) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(role)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn get_user_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?1")
        .bind(username)
        .fetch_optional(pool)
        .await
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY id")
        .fetch_all(pool)
        .await
}

pub async fn update_user(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password_hash: Option<&str>,
    role: Option<&str>,
) -> Result<(), sqlx::Error> {
    // Build dynamic update — only update fields that are provided
    if let Some(email_val) = email {
        sqlx::query("UPDATE users SET email = ?1, updated_at = datetime('now') WHERE username = ?2")
            .bind(email_val)
            .bind(username)
            .execute(pool)
            .await?;
    }
    if let Some(hash) = password_hash {
        sqlx::query(
            "UPDATE users SET password_hash = ?1, updated_at = datetime('now') WHERE username = ?2",
        )
        .bind(hash)
        .bind(username)
        .execute(pool)
        .await?;
    }
    if let Some(role_val) = role {
        sqlx::query("UPDATE users SET role = ?1, updated_at = datetime('now') WHERE username = ?2")
            .bind(role_val)
            .bind(username)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn delete_user(pool: &SqlitePool, username: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM users WHERE username = ?1")
        .bind(username)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_must_change_password(
    pool: &SqlitePool,
    user_id: i64,
    value: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET must_change_password = ?1, updated_at = datetime('now') WHERE id = ?2")
        .bind(if value { 1i64 } else { 0i64 })
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// API token CRUD
// ---------------------------------------------------------------------------

pub async fn create_api_token(
    pool: &SqlitePool,
    id: &str,
    user_id: i64,
    name: &str,
    prefix: &str,
    token_hash: &str,
    expires_at: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, prefix, token_hash, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(prefix)
    .bind(token_hash)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_token_by_prefix(
    pool: &SqlitePool,
    prefix: &str,
) -> Result<Option<ApiToken>, sqlx::Error> {
    sqlx::query_as::<_, ApiToken>("SELECT * FROM api_tokens WHERE prefix = ?1")
        .bind(prefix)
        .fetch_optional(pool)
        .await
}

pub async fn list_user_tokens(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<ApiToken>, sqlx::Error> {
    sqlx::query_as::<_, ApiToken>(
        "SELECT * FROM api_tokens WHERE user_id = ?1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn get_token_by_id(
    pool: &SqlitePool,
    token_id: &str,
) -> Result<Option<ApiToken>, sqlx::Error> {
    sqlx::query_as::<_, ApiToken>("SELECT * FROM api_tokens WHERE id = ?1")
        .bind(token_id)
        .fetch_optional(pool)
        .await
}

pub async fn delete_token(pool: &SqlitePool, token_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_tokens WHERE id = ?1")
        .bind(token_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_token_last_used(pool: &SqlitePool, token_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_tokens SET last_used_at = datetime('now') WHERE id = ?1")
        .bind(token_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn create_audit_entry(
    pool: &SqlitePool,
    user_id: Option<i64>,
    username: Option<&str>,
    action: &str,
    target: Option<&str>,
    repository: Option<&str>,
    ip: Option<&str>,
    user_agent: Option<&str>,
    details_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (user_id, username, action, target, repository, ip, user_agent, details_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(user_id)
    .bind(username)
    .bind(action)
    .bind(target)
    .bind(repository)
    .bind(ip)
    .bind(user_agent)
    .bind(details_json)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Package dependencies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct PackageDependency {
    pub id: i64,
    pub package_id: i64,
    pub version_id: i64,
    pub dependency_name: String,
    pub dependency_version_req: String,
    pub dependency_type: String,
    pub created_at: String,
}

pub async fn insert_dependency(
    pool: &SqlitePool,
    package_id: i64,
    version_id: i64,
    dep_name: &str,
    dep_version_req: &str,
    dep_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO package_dependencies (package_id, version_id, dependency_name, dependency_version_req, dependency_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(package_id)
    .bind(version_id)
    .bind(dep_name)
    .bind(dep_version_req)
    .bind(dep_type)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get all dependencies for a given package (latest version or all versions).
pub async fn get_dependencies_for_package(
    pool: &SqlitePool,
    package_id: i64,
) -> Result<Vec<PackageDependency>, sqlx::Error> {
    sqlx::query_as::<_, PackageDependency>(
        "SELECT * FROM package_dependencies WHERE package_id = ?1 ORDER BY id",
    )
    .bind(package_id)
    .fetch_all(pool)
    .await
}

/// Get all dependencies for a specific version.
pub async fn get_dependencies_for_version(
    pool: &SqlitePool,
    version_id: i64,
) -> Result<Vec<PackageDependency>, sqlx::Error> {
    sqlx::query_as::<_, PackageDependency>(
        "SELECT * FROM package_dependencies WHERE version_id = ?1 ORDER BY id",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await
}

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct DependentInfo {
    pub name: String,
    pub version: String,
}

/// Get all packages that depend on the given dependency name.
pub async fn get_dependents(
    pool: &SqlitePool,
    dependency_name: &str,
) -> Result<Vec<DependentInfo>, sqlx::Error> {
    sqlx::query_as::<_, DependentInfo>(
        "SELECT DISTINCT p.name, v.version FROM package_dependencies d \
         JOIN versions v ON d.version_id = v.id \
         JOIN packages p ON d.package_id = p.id \
         WHERE d.dependency_name = ?1",
    )
    .bind(dependency_name)
    .fetch_all(pool)
    .await
}

/// Get all packages that depend on a specific version of a dependency.
/// Used for impact analysis — checks if the version_req would match.
pub async fn get_dependents_of_version(
    pool: &SqlitePool,
    dependency_name: &str,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows: Vec<DependentInfo> = sqlx::query_as(
        "SELECT DISTINCT p.name, v.version FROM package_dependencies d \
         JOIN versions v ON d.version_id = v.id \
         JOIN packages p ON d.package_id = p.id \
         WHERE d.dependency_name = ?1",
    )
    .bind(dependency_name)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| (r.name, r.version)).collect())
}

pub async fn list_audit_entries(
    pool: &SqlitePool,
    page: i64,
    size: i64,
) -> Result<Vec<AuditEntry>, sqlx::Error> {
    let offset = (page - 1).max(0) * size;
    sqlx::query_as::<_, AuditEntry>(
        "SELECT * FROM audit_log ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
    )
    .bind(size)
    .bind(offset)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Vulnerability scans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct VulnerabilityScan {
    pub id: i64,
    pub version_id: i64,
    pub scanned_at: String,
    pub total_deps: i64,
    pub vulnerable_deps: i64,
    pub scan_results_json: Option<String>,
    pub status: String,
}

pub async fn insert_vulnerability_scan(
    pool: &SqlitePool,
    version_id: i64,
    total_deps: i64,
    vulnerable_deps: i64,
    scan_results_json: Option<&str>,
    status: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO vulnerability_scans (version_id, total_deps, vulnerable_deps, scan_results_json, status)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(version_id)
    .bind(total_deps)
    .bind(vulnerable_deps)
    .bind(scan_results_json)
    .bind(status)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn get_vulnerability_scan(
    pool: &SqlitePool,
    version_id: i64,
) -> Result<Option<VulnerabilityScan>, sqlx::Error> {
    sqlx::query_as::<_, VulnerabilityScan>(
        "SELECT * FROM vulnerability_scans WHERE version_id = ?1 ORDER BY scanned_at DESC LIMIT 1",
    )
    .bind(version_id)
    .fetch_optional(pool)
    .await
}

pub async fn delete_vulnerability_scans(
    pool: &SqlitePool,
    version_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM vulnerability_scans WHERE version_id = ?1")
        .bind(version_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository CRUD
// ---------------------------------------------------------------------------

pub async fn get_all_repositories(pool: &SqlitePool) -> Result<Vec<Repository>, sqlx::Error> {
    sqlx::query_as::<_, Repository>("SELECT * FROM repositories ORDER BY name")
        .fetch_all(pool)
        .await
}

pub async fn create_repository(
    pool: &SqlitePool,
    name: &str,
    repo_type: &str,
    format: &str,
    visibility: &str,
    upstream_url: Option<&str>,
    config_json: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO repositories (name, repo_type, format, visibility, upstream_url, config_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(name)
    .bind(repo_type)
    .bind(format)
    .bind(visibility)
    .bind(upstream_url)
    .bind(config_json)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn update_repository(
    pool: &SqlitePool,
    name: &str,
    visibility: Option<&str>,
    upstream_url: Option<&str>,
    config_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    if let Some(vis) = visibility {
        sqlx::query(
            "UPDATE repositories SET visibility = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(vis)
        .bind(name)
        .execute(pool)
        .await?;
    }
    if let Some(url) = upstream_url {
        sqlx::query(
            "UPDATE repositories SET upstream_url = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(url)
        .bind(name)
        .execute(pool)
        .await?;
    }
    if let Some(cfg) = config_json {
        sqlx::query(
            "UPDATE repositories SET config_json = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(cfg)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn delete_repository(pool: &SqlitePool, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM repositories WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// User permissions CRUD
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UserPermission {
    pub id: i64,
    pub user_id: i64,
    pub repository_id: i64,
    pub can_read: i64,
    pub can_write: i64,
    pub can_delete: i64,
    pub can_admin: i64,
    pub created_at: String,
}

pub async fn get_user_permission(
    pool: &SqlitePool,
    user_id: i64,
    repository_id: i64,
) -> Result<Option<UserPermission>, sqlx::Error> {
    sqlx::query_as::<_, UserPermission>(
        "SELECT * FROM user_permissions WHERE user_id = ?1 AND repository_id = ?2",
    )
    .bind(user_id)
    .bind(repository_id)
    .fetch_optional(pool)
    .await
}

pub async fn list_user_permissions(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<UserPermission>, sqlx::Error> {
    sqlx::query_as::<_, UserPermission>(
        "SELECT * FROM user_permissions WHERE user_id = ?1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn set_user_permission(
    pool: &SqlitePool,
    user_id: i64,
    repository_id: i64,
    can_read: bool,
    can_write: bool,
    can_delete: bool,
    can_admin: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_permissions (user_id, repository_id, can_read, can_write, can_delete, can_admin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(user_id, repository_id) DO UPDATE SET
           can_read = excluded.can_read,
           can_write = excluded.can_write,
           can_delete = excluded.can_delete,
           can_admin = excluded.can_admin",
    )
    .bind(user_id)
    .bind(repository_id)
    .bind(if can_read { 1i64 } else { 0i64 })
    .bind(if can_write { 1i64 } else { 0i64 })
    .bind(if can_delete { 1i64 } else { 0i64 })
    .bind(if can_admin { 1i64 } else { 0i64 })
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_user_permission(
    pool: &SqlitePool,
    user_id: i64,
    repository_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM user_permissions WHERE user_id = ?1 AND repository_id = ?2")
        .bind(user_id)
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook CRUD (DB-backed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub events: String,
    pub secret: Option<String>,
    pub active: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn list_webhooks(pool: &SqlitePool) -> Result<Vec<Webhook>, sqlx::Error> {
    sqlx::query_as::<_, Webhook>("SELECT * FROM webhooks ORDER BY id")
        .fetch_all(pool)
        .await
}

pub async fn get_webhook_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<Webhook>, sqlx::Error> {
    sqlx::query_as::<_, Webhook>("SELECT * FROM webhooks WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn create_webhook(
    pool: &SqlitePool,
    url: &str,
    events: &str,
    secret: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO webhooks (url, events, secret) VALUES (?1, ?2, ?3)",
    )
    .bind(url)
    .bind(events)
    .bind(secret)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn update_webhook(
    pool: &SqlitePool,
    id: i64,
    url: Option<&str>,
    events: Option<&str>,
    secret: Option<Option<&str>>,
    active: Option<bool>,
) -> Result<(), sqlx::Error> {
    if let Some(url_val) = url {
        sqlx::query("UPDATE webhooks SET url = ?1, updated_at = datetime('now') WHERE id = ?2")
            .bind(url_val)
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(events_val) = events {
        sqlx::query("UPDATE webhooks SET events = ?1, updated_at = datetime('now') WHERE id = ?2")
            .bind(events_val)
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(secret_opt) = secret {
        sqlx::query("UPDATE webhooks SET secret = ?1, updated_at = datetime('now') WHERE id = ?2")
            .bind(secret_opt)
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(active_val) = active {
        sqlx::query("UPDATE webhooks SET active = ?1, updated_at = datetime('now') WHERE id = ?2")
            .bind(if active_val { 1i64 } else { 0i64 })
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn delete_webhook(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM webhooks WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_active_webhooks(
    pool: &SqlitePool,
    event: &str,
) -> Result<Vec<Webhook>, sqlx::Error> {
    // Return webhooks that are active and whose events match the given event
    // events is stored as comma-separated or "*"
    let all_webhooks = sqlx::query_as::<_, Webhook>(
        "SELECT * FROM webhooks WHERE active = 1",
    )
    .fetch_all(pool)
    .await?;

    Ok(all_webhooks
        .into_iter()
        .filter(|wh| {
            let events = &wh.events;
            events == "*"
                || events
                    .split(',')
                    .any(|e| e.trim() == event)
        })
        .collect())
}

/// Seed webhooks from config into the DB (only inserts if no webhooks exist yet).
pub async fn seed_webhooks(
    pool: &SqlitePool,
    webhooks: &[crate::config::WebhookConfig],
) -> Result<(), sqlx::Error> {
    if webhooks.is_empty() {
        return Ok(());
    }

    // Only seed if there are no webhooks in the DB yet
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM webhooks")
        .fetch_one(pool)
        .await?;

    if count > 0 {
        return Ok(());
    }

    for wh in webhooks {
        let events = if wh.events.is_empty() {
            "*".to_string()
        } else {
            wh.events.join(",")
        };
        create_webhook(pool, &wh.url, &events, wh.secret.as_deref()).await?;
    }

    Ok(())
}
