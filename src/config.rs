use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::rules::PolicyConfig;
use crate::proxy::UpstreamAuth;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub proxy: ProxyConfig,
    pub cleanup: CleanupConfig,
    #[serde(default)]
    pub repositories: Vec<RepositoryConfig>,
    #[serde(default)]
    pub webhooks: Vec<WebhookConfig>,
    #[serde(default)]
    pub vuln_scan: VulnScanConfig,
    /// Policy rules per proxy repository name; a member with none on records nothing.
    #[serde(default)]
    pub policy: HashMap<String, PolicyConfig>,
    #[serde(default)]
    pub routing: RoutingConfig,
}

// ---------------------------------------------------------------------------
// Group routing rules
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RoutingConfig {
    /// How often a node re-reads the rules.
    pub refresh_secs: u64,
    /// How long a node serves a snapshot nothing has refreshed. Past it the
    /// proxy members of every format a rule speaks for are refused, rather
    /// than serving the state from before a rule this node may not have seen.
    /// Several refresh periods, so a single failed read is not an outage.
    pub max_snapshot_age_secs: u64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 30,
            max_snapshot_age_secs: 300,
        }
    }
}

impl RoutingConfig {
    pub fn refresh(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_secs)
    }

    pub fn max_snapshot_age(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.max_snapshot_age_secs)
    }
}

// ---------------------------------------------------------------------------
// Vulnerability scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct VulnScanConfig {
    pub enabled: bool,
    pub block_on_critical: bool,
    pub osv_base_url: String,
    /// With `block_on_critical`, an OSV outage refuses the publish (503)
    /// instead of letting it through unscanned.
    pub fail_closed: bool,
    pub max_concurrency: usize,
}

impl Default for VulnScanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            block_on_critical: false,
            osv_base_url: "https://api.osv.dev".to_string(),
            fail_closed: false,
            max_concurrency: 8,
        }
    }
}

// ---------------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebhookConfig {
    pub url: String,
    pub events: Vec<String>,
    pub secret: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub base_url: String,
    pub storage_path: String,
    pub tls: TlsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:6789".to_string(),
            base_url: "http://localhost:6789".to_string(),
            storage_path: "./data/storage".to_string(),
            tls: TlsConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    pub enabled: bool,
    pub cert_path: String,
    pub key_path: String,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    #[default]
    Fs,
    S3,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: StorageKind,
    /// The store's declared identity; its role's name when absent. Never an
    /// endpoint, a bucket or a path.
    pub id: Option<String>,
    pub s3: S3Config,
}

/// Credentials are never read from here: only from the environment, through
/// the S3 adapter's allowlist.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    /// Isolates this instance inside a shared bucket.
    pub prefix: String,
    pub allow_http: bool,
    pub virtual_hosted_style: bool,
    /// How long one request may stay idle.
    pub request_timeout: String,
    /// The outer bound of a multipart completion or a server-side copy.
    pub completion_timeout: String,
    pub part_size_mib: u64,
    /// Server-side multipart uploads open at once, process-wide.
    pub max_multipart_uploads: usize,
    /// Positive existence answers kept for `head`; 0 disables the cache.
    pub exists_cache_entries: usize,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            region: "us-east-1".to_string(),
            endpoint: None,
            prefix: String::new(),
            allow_http: false,
            virtual_hosted_style: false,
            request_timeout: "30s".to_string(),
            completion_timeout: "15m".to_string(),
            part_size_mib: 16,
            max_multipart_uploads: 8,
            exists_cache_entries: 10_000,
        }
    }
}

/// `30s`, `15m`, `2h`, or a bare number of seconds.
pub fn parse_duration(value: &str) -> Result<std::time::Duration> {
    let value = value.trim();
    let (digits, unit) = value.split_at(value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len()));
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid duration: '{value}'"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => anyhow::bail!("invalid duration unit in '{value}'"),
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// A prefix as the S3 adapter uses it: segments joined by `/`, none empty,
/// none `.` or `..`, so two spellings of one prefix cannot alias.
pub fn normalize_prefix(prefix: &str) -> Result<String> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.starts_with('_') {
            anyhow::bail!("invalid storage prefix: '{prefix}'");
        }
    }
    Ok(trimmed.to_string())
}

/// Two stores one process builds may not share an identity: the ledger
/// scopes its rows by it.
pub fn refuse_duplicate_identities<'a>(identities: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for id in identities {
        if !seen.insert(id) {
            anyhow::bail!("two stores are declared with the identity '{id}'");
        }
    }
    Ok(())
}

impl Config {
    /// The identity of every store this process builds.
    pub fn store_identities(&self) -> Vec<String> {
        vec![self.storage.id.clone().unwrap_or_else(|| "artifacts".to_string())]
    }

    /// What a config must satisfy before any store is built.
    pub fn validate(&self) -> Result<()> {
        let ids = self.store_identities();
        if ids.iter().any(|id| id.trim().is_empty()) {
            anyhow::bail!("a storage id may not be empty");
        }
        refuse_duplicate_identities(ids.iter().map(String::as_str))?;
        if self.storage.backend == StorageKind::S3 {
            let s3 = &self.storage.s3;
            if s3.bucket.trim().is_empty() && std::env::var("OPENCARGO_S3_BUCKET").is_err() {
                anyhow::bail!("[storage.s3] bucket is required");
            }
            normalize_prefix(&s3.prefix)?;
            parse_duration(&s3.request_timeout)?;
            parse_duration(&s3.completion_timeout)?;
            if s3.part_size_mib < 5 {
                anyhow::bail!("[storage.s3] part_size_mib must be at least 5");
            }
            if s3.max_multipart_uploads == 0 {
                anyhow::bail!("[storage.s3] max_multipart_uploads must be at least 1");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "sqlite:./data/db/opencargo.db".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub anonymous_read: bool,
    pub token_prefix: String,
    pub static_tokens: Vec<String>,
    pub admin: AdminConfig,
    /// Peers whose `X-Forwarded-For` names the client for the token limiter.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    pub sso: SsoConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SsoConfig {
    /// `enabled`, `admins_only` or `disabled`.
    pub password_mode: String,
    pub session_ttl: String,
    /// Empty: an SSO user's credentials never need a fresh login.
    pub reauth_after: String,
    pub reauth_grace_max: String,
    pub handoff_ttl: String,
    pub probe_interval: String,
    /// Plain-HTTP cookies for a loopback development server only.
    pub dev_insecure_http: bool,
    pub providers: Vec<SsoProviderConfig>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SsoProviderConfig {
    pub name: String,
    /// `google`, `entra`, `gitlab` or `generic`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Required for `gitlab` and `generic`; an Entra issuer template with
    /// `{tid}` overrides Microsoft's.
    pub issuer: String,
    /// Entra only: a tenant id, or `common` / `organizations`.
    pub tenant: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    pub groups_claim: Option<String>,
    pub open: Option<bool>,
    pub allow_open: bool,
    pub authoritative_domains: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub required_groups: Vec<String>,
    pub default_role: Option<String>,
    pub grants: Vec<SsoGrantConfig>,
    /// Declares a provider removed: its credentials are revoked at startup.
    pub retired: bool,
    /// Declares the issuer this provider was known under before.
    pub issuer_was: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SsoGrantConfig {
    pub group: String,
    pub repository: String,
    pub role: String,
}

impl Default for SsoConfig {
    fn default() -> Self {
        Self {
            password_mode: "enabled".to_string(),
            session_ttl: "12h".to_string(),
            reauth_after: String::new(),
            reauth_grace_max: "72h".to_string(),
            handoff_ttl: "120s".to_string(),
            probe_interval: "60s".to_string(),
            dev_insecure_http: false,
            providers: Vec::new(),
        }
    }
}

/// `90s`, `15m`, `12h`, `30d` or bare seconds; anything else is refused
/// rather than defaulted.
pub fn parse_chrono_duration(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    let (n, unit) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&s[..i], c),
        _ => (s, 's'),
    };
    let n: i64 = n
        .parse()
        .with_context(|| format!("invalid duration {s:?}"))?;
    let secs = match unit {
        's' => n,
        'm' => n * 60,
        'h' => n * 3600,
        'd' => n * 86400,
        _ => anyhow::bail!("invalid duration unit in {s:?}"),
    };
    anyhow::ensure!(secs >= 0, "negative duration {s:?}");
    Ok(chrono::Duration::seconds(secs))
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            token_prefix: "trg_".to_string(),
            static_tokens: Vec::new(),
            admin: AdminConfig::default(),
            trusted_proxies: Vec::new(),
            sso: SsoConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    pub username: String,
    pub password: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            password: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub default_ttl: String,
    pub negative_cache_ttl: String,
    pub connect_timeout: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            default_ttl: "24h".to_string(),
            negative_cache_ttl: "1h".to_string(),
            connect_timeout: "10s".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub prerelease_older_than_days: Option<u64>,
    /// Proxy cache rows idle this long are evicted; runs regardless of `enabled`.
    pub proxy_cache_older_than_days: Option<u64>,
    /// Policy report rows older than this are purged; runs regardless of `enabled`.
    pub policy_report_older_than_days: Option<u64>,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prerelease_older_than_days: None,
            proxy_cache_older_than_days: Some(30),
            policy_report_older_than_days: Some(90),
        }
    }
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct RepositoryConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub repo_type: RepositoryType,
    pub format: RepositoryFormat,
    #[serde(default)]
    pub visibility: Visibility,
    pub upstream: Option<String>,
    pub members: Option<Vec<String>>,
    /// Upstream credentials; config and environment only, never the API.
    pub upstream_auth: Option<UpstreamAuth>,
    /// Token realms off the upstream host that may still see `upstream_auth`.
    #[serde(default)]
    pub token_realms: Vec<String>,
    /// Let an upstream-chosen download URL point at a private IP literal.
    #[serde(default)]
    pub dl_allow_private: bool,
    /// Hosts a PyPI upstream's pages may point file downloads at.
    #[serde(default)]
    pub file_hosts: Vec<String>,
}

/// Aliases kept so existing config and test code keep compiling; the types
/// themselves are registry vocabulary and live in the domain.
pub type RepositoryType = crate::domain::RepoKind;
pub type RepositoryFormat = crate::domain::Format;
pub use crate::domain::Visibility;

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load configuration from an explicit path, well-known locations, or defaults.
///
/// Resolution order:
/// 1. Explicit `path` argument (error if it does not exist).
/// 2. `./config.toml` in the current directory.
/// 3. `~/.opencargo/config.toml`.
/// 4. Built-in defaults.
pub fn load_config(path: Option<&Path>) -> Result<Config> {
    if let Some(p) = path {
        let content = std::fs::read_to_string(p)
            .with_context(|| format!("failed to read config file: {}", p.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("failed to parse config file: {}", p.display()))?;
        return Ok(config);
    }

    // Try well-known locations.
    let candidates: Vec<std::path::PathBuf> = {
        let mut v = vec![std::path::PathBuf::from("config.toml")];
        if let Some(home) = dirs::home_dir() {
            v.push(home.join(".opencargo").join("config.toml"));
        }
        v
    };

    for candidate in &candidates {
        if candidate.is_file() {
            let content = std::fs::read_to_string(candidate).with_context(|| {
                format!("failed to read config file: {}", candidate.display())
            })?;
            let config: Config = toml::from_str(&content).with_context(|| {
                format!("failed to parse config file: {}", candidate.display())
            })?;
            return Ok(config);
        }
    }

    // Nothing found -- return defaults.
    Ok(Config::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_storage_section_takes_documented_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.storage.backend, StorageKind::Fs);
        assert_eq!(config.storage.id, None);
        let s3 = &config.storage.s3;
        assert_eq!((s3.request_timeout.as_str(), s3.completion_timeout.as_str()), ("30s", "15m"));
        assert_eq!((s3.part_size_mib, s3.max_multipart_uploads, s3.exists_cache_entries), (16, 8, 10_000));
        assert_eq!(s3.region, "us-east-1");
        config.validate().unwrap();

        let partial: Config = toml::from_str("[storage]\nbackend = \"s3\"\n[storage.s3]\nbucket = \"b\"\n").unwrap();
        assert_eq!(partial.storage.s3.request_timeout, "30s", "a partial section keeps the defaults");
        partial.validate().unwrap();
    }

    #[test]
    fn validate_refuses_duplicate_store_identities() {
        assert!(refuse_duplicate_identities(["artifacts", "backup"]).is_ok());
        assert!(refuse_duplicate_identities(["artifacts", "artifacts"]).is_err());
        let blank: Config = toml::from_str("[storage]\nid = \" \"\n").unwrap();
        assert!(blank.validate().is_err());
    }

    #[test]
    fn default_identity_is_the_role() {
        assert_eq!(Config::default().store_identities(), vec!["artifacts".to_string()]);
    }

    #[test]
    fn prefixes_are_normalized_so_two_spellings_never_alias() {
        assert_eq!(normalize_prefix("/team/a/").unwrap(), "team/a");
        assert_eq!(normalize_prefix("").unwrap(), "");
        for bad in ["a//b", "a/../b", "./a", "_scratch", "a/_backend"] {
            assert!(normalize_prefix(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_duration("15m").unwrap().as_secs(), 900);
        assert_eq!(parse_duration("45").unwrap().as_secs(), 45);
        assert!(parse_duration("3d").is_err());
    }
}
