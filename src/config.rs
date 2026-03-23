use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            auth: AuthConfig::default(),
            proxy: ProxyConfig::default(),
            cleanup: CleanupConfig::default(),
            repositories: Vec::new(),
            webhooks: Vec::new(),
            vuln_scan: VulnScanConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Vulnerability scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct VulnScanConfig {
    pub enabled: bool,
    pub block_on_critical: bool,
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
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            token_prefix: "trg_".to_string(),
            static_tokens: Vec::new(),
            admin: AdminConfig::default(),
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

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub prerelease_older_than_days: Option<u64>,
    pub proxy_cache_older_than_days: Option<u64>,
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RepositoryConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub repo_type: RepositoryType,
    pub format: RepositoryFormat,
    #[serde(default)]
    pub visibility: Visibility,
    pub upstream: Option<String>,
    pub members: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryType {
    Hosted,
    Proxy,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryFormat {
    Npm,
    Cargo,
    Oci,
    Go,
    Pypi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

impl Default for Visibility {
    fn default() -> Self {
        Self::Private
    }
}

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
