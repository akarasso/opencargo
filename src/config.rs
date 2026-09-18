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
pub fn parse_duration(s: &str) -> Result<chrono::Duration> {
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
