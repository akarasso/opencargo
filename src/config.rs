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
    pub backup: BackupConfig,
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
    /// Take the writer lease before writing anything. Off is for tests: two
    /// instances on one database are not supported.
    pub lease: bool,
    pub lease_wait: String,
    pub lease_stale_after: String,
    pub lease_renew: String,
    /// How long in-flight requests may run once the drain starts.
    pub shutdown_grace: String,
    /// How long `/health/ready` answers 503 while still serving, before the
    /// drain starts: one poll period of an ingress or mesh that polls.
    pub endpoint_drain: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:6789".to_string(),
            base_url: "http://localhost:6789".to_string(),
            storage_path: "./data/storage".to_string(),
            tls: TlsConfig::default(),
            lease: true,
            lease_wait: "60s".to_string(),
            lease_stale_after: "30s".to_string(),
            lease_renew: "10s".to_string(),
            shutdown_grace: "30s".to_string(),
            endpoint_drain: "0s".to_string(),
        }
    }
}

impl ServerConfig {
    /// The lease this server takes, `None` when it takes none.
    pub fn lease_terms(&self) -> Result<Option<crate::app::lease::LeaseTerms>> {
        if !self.lease {
            return Ok(None);
        }
        Ok(Some(crate::app::lease::LeaseTerms {
            wait: parse_duration(&self.lease_wait)?,
            stale_after: parse_duration(&self.lease_stale_after)?,
            renew: parse_duration(&self.lease_renew)?,
        }))
    }

    pub fn shutdown_grace(&self) -> Result<std::time::Duration> {
        parse_duration(&self.shutdown_grace)
    }

    pub fn endpoint_drain(&self) -> Result<std::time::Duration> {
        parse_duration(&self.endpoint_drain)
    }

    fn problems(&self, problems: &mut Vec<String>) {
        let mut parsed = |key: &str, value: &str| match parse_duration(value) {
            Ok(d) => Some(d),
            Err(_) => {
                problems.push(format!("[server] {key} = {value:?} is not a duration (30s, 15m, 2h or seconds)"));
                None
            }
        };
        let wait = parsed("lease_wait", &self.lease_wait);
        let stale = parsed("lease_stale_after", &self.lease_stale_after);
        let renew = parsed("lease_renew", &self.lease_renew);
        let grace = parsed("shutdown_grace", &self.shutdown_grace);
        parsed("endpoint_drain", &self.endpoint_drain);
        if let (Some(renew), Some(stale)) = (renew, stale) {
            if renew.is_zero() || renew * 3 > stale {
                problems.push(
                    "[server] lease_renew must be non-zero and three renewals must fit in lease_stale_after".to_string(),
                );
            }
        }
        if let (Some(wait), Some(stale)) = (wait, stale) {
            if wait <= stale {
                problems.push(
                    "[server] lease_wait must be longer than lease_stale_after, or a restart dies waiting on its own lease"
                        .to_string(),
                );
            }
        }
        if grace.is_some_and(|g| g < std::time::Duration::from_secs(1)) {
            problems.push("[server] shutdown_grace must be at least 1s".to_string());
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
// Backup
// ---------------------------------------------------------------------------

/// The in-process backup schedule. `storage` is off for the scheduler: seven
/// full artifact copies on the data volume would fill it.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BackupConfig {
    pub enabled: bool,
    pub every: String,
    /// `HH:MM`, UTC: the phase of `every`.
    pub at: String,
    pub keep: usize,
    /// A local directory.
    pub to: String,
    pub storage: bool,
    /// An off-box copy of every finished snapshot, in a keyspace disjoint
    /// from `[storage]`; built by the run, never at boot.
    pub sink: Option<BackupSinkConfig>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            every: "24h".to_string(),
            at: "03:00".to_string(),
            keep: 7,
            to: String::new(),
            storage: false,
            sink: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct BackupSinkConfig {
    #[serde(flatten)]
    pub storage: StorageConfig,
    /// The root of a filesystem sink.
    pub path: String,
}

impl BackupConfig {
    fn problems(&self, config: &Config, problems: &mut Vec<String>) {
        if self.enabled && self.to.trim().is_empty() {
            problems.push("[backup] enabled needs `to`: a schedule with nowhere to write never backs up".to_string());
        }
        match parse_duration(&self.every) {
            Ok(every) if every.as_secs() < 3600 => problems.push("[backup] every must be at least 1h".to_string()),
            Ok(every) if 86_400 % every.as_secs() != 0 => problems.push(format!(
                "[backup] every = {:?} does not divide the day, so `at` has no single phase",
                self.every
            )),
            Ok(_) => {}
            Err(_) => problems.push(format!("[backup] every = {:?} is not a duration", self.every)),
        }
        if crate::backup::schedule::parse_at(&self.at).is_none() {
            problems.push(format!("[backup] at = {:?} is not HH:MM (UTC)", self.at));
        }
        if self.keep == 0 {
            problems.push("[backup] keep must be at least 1".to_string());
        }
        if let Some(sink) = &self.sink {
            if !sink_disjoint(sink, config) {
                problems.push(
                    "[backup.sink] overlaps [storage]: a snapshot would land among the artifacts".to_string(),
                );
            }
        }
    }
}

/// The declared-TOML check only: the resolved one runs with each backup.
fn sink_disjoint(sink: &BackupSinkConfig, config: &Config) -> bool {
    match (sink.storage.backend, config.storage.backend) {
        (StorageKind::S3, StorageKind::S3) => {
            let (a, b) = (&sink.storage.s3, &config.storage.s3);
            (a.endpoint.as_deref(), a.region.as_str(), a.bucket.as_str()) != (b.endpoint.as_deref(), b.region.as_str(), b.bucket.as_str())
                || !segments_overlap(&a.prefix, &b.prefix, '/')
        }
        (StorageKind::Fs, StorageKind::Fs) => {
            !segments_overlap(&sink.path, &config.server.storage_path, std::path::MAIN_SEPARATOR)
        }
        _ => true,
    }
}

/// One of the two is the other or lies under it, on segment boundaries.
pub fn segments_overlap(a: &str, b: &str, sep: char) -> bool {
    let (a, b) = (a.trim_end_matches(sep), b.trim_end_matches(sep));
    let under = |x: &str, y: &str| y.is_empty() || x == y || x.starts_with(&format!("{y}{sep}"));
    under(a, b) || under(b, a)
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
        let mut ids = vec![self.storage.id.clone().unwrap_or_else(|| "artifacts".to_string())];
        if let Some(sink) = &self.backup.sink {
            ids.push(sink.storage.id.clone().unwrap_or_else(|| "backup".to_string()));
        }
        ids
    }

    /// What a config must satisfy before any store is built, every refusal
    /// at once.
    pub fn validate(&self) -> Result<()> {
        let problems = self.problems();
        if problems.is_empty() {
            return Ok(());
        }
        anyhow::bail!("invalid configuration:\n  {}", problems.join("\n  "))
    }

    /// Every rule this config breaks. The `[proxy]` durations are not read:
    /// they fall back to 10s as they always have.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let ids = self.store_identities();
        if ids.iter().any(|id| id.trim().is_empty()) {
            problems.push("a storage id may not be empty".to_string());
        }
        if let Err(e) = refuse_duplicate_identities(ids.iter().map(String::as_str)) {
            problems.push(e.to_string());
        }
        if self.storage.backend == StorageKind::S3 {
            let s3 = &self.storage.s3;
            if s3.bucket.trim().is_empty() && std::env::var("OPENCARGO_S3_BUCKET").is_err() {
                problems.push("[storage.s3] bucket is required".to_string());
            }
            for outcome in [
                normalize_prefix(&s3.prefix).map(|_| ()),
                parse_duration(&s3.request_timeout).map(|_| ()),
                parse_duration(&s3.completion_timeout).map(|_| ()),
            ] {
                if let Err(e) = outcome {
                    problems.push(format!("[storage.s3] {e}"));
                }
            }
            if s3.part_size_mib < 5 {
                problems.push("[storage.s3] part_size_mib must be at least 5".to_string());
            }
            if s3.max_multipart_uploads == 0 {
                problems.push("[storage.s3] max_multipart_uploads must be at least 1".to_string());
            }
        }
        self.server.problems(&mut problems);
        self.backup.problems(self, &mut problems);
        problems
    }

    /// The three keys the deployment manifests derive their grace period
    /// from, as the manifests render them.
    fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        for (name, field) in [
            ("OPENCARGO_LEASE_WAIT", &mut self.server.lease_wait),
            ("OPENCARGO_SHUTDOWN_GRACE", &mut self.server.shutdown_grace),
            ("OPENCARGO_ENDPOINT_DRAIN", &mut self.server.endpoint_drain),
        ] {
            if let Some(value) = var(name) {
                *field = value;
            }
        }
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

/// A config as loaded, and every rule it breaks: whether a problem is fatal
/// is the caller's decision, so a bad config never disables the commands
/// that recover from it.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub problems: Vec<String>,
}

/// Load configuration, apply the environment's overrides, then validate the
/// effective values.
///
/// Resolution order:
/// 1. Explicit `path` argument (error if it does not exist).
/// 2. `./config.toml` in the current directory.
/// 3. `~/.opencargo/config.toml`.
/// 4. Built-in defaults.
pub fn load_config(path: Option<&Path>) -> Result<Loaded> {
    let mut config = read_config(path)?;
    config.apply_env(|name| std::env::var(name).ok());
    let problems = config.problems();
    Ok(Loaded { config, problems })
}

fn read_config(path: Option<&Path>) -> Result<Config> {
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

    #[test]
    fn duration_parse_table() {
        for (input, want) in [("30", Some(30)), ("10s", Some(10)), ("24h", Some(86_400)), ("15m", Some(900))] {
            assert_eq!(parse_duration(input).ok().map(|d| d.as_secs()), want, "{input}");
        }
        for refused in ["1d", "5min", "03:00", "", "s", "-1s"] {
            assert!(parse_duration(refused).is_err(), "{refused:?}");
        }
    }

    fn problems_of(toml: &str) -> Vec<String> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, toml).unwrap();
        load_config(Some(&path)).unwrap().problems
    }

    #[test]
    fn validate_rules_table() {
        let rows: &[(&str, &str)] = &[
            ("[server]\nlease_renew = \"20s\"", "three renewals"),
            ("[server]\nlease_wait = \"30s\"", "lease_wait must be longer"),
            ("[server]\nshutdown_grace = \"0s\"", "shutdown_grace must be at least 1s"),
            ("[server]\nendpoint_drain = \"soon\"", "endpoint_drain"),
            ("[server]\nlease_stale_after = \"5min\"", "lease_stale_after"),
        ];
        for (toml, needle) in rows {
            let problems = problems_of(toml);
            assert!(problems.iter().any(|p| p.contains(needle)), "{toml}: {problems:?}");
        }
        assert!(problems_of("").is_empty());
    }

    #[test]
    fn a_garbage_proxy_duration_still_loads() {
        for ttl in ["5min", "1d"] {
            assert!(problems_of(&format!("[proxy]\ndefault_ttl = \"{ttl}\"")).is_empty(), "{ttl}");
        }
    }

    #[test]
    fn env_overrides_the_file_and_is_validated() {
        let mut config: Config = toml::from_str("[server]\nshutdown_grace = \"45s\"").unwrap();
        config.apply_env(|name| match name {
            "OPENCARGO_SHUTDOWN_GRACE" => Some("0".to_string()),
            "OPENCARGO_LEASE_WAIT" => Some("90".to_string()),
            _ => None,
        });
        assert_eq!(config.server.shutdown_grace, "0");
        assert_eq!(config.server.lease_terms().unwrap().unwrap().wait.as_secs(), 90);
        assert!(config.problems().iter().any(|p| p.contains("shutdown_grace")));
    }

    #[test]
    fn backup_rules_table() {
        let rows: &[(&str, &str)] = &[
            ("[backup]\nenabled = true", "enabled needs `to`"),
            ("[backup]\nevery = \"7h\"", "does not divide the day"),
            ("[backup]\nevery = \"30m\"", "at least 1h"),
            ("[backup]\nevery = \"1d\"", "not a duration"),
            ("[backup]\nat = \"3am\"", "HH:MM"),
            ("[backup]\nkeep = 0", "keep"),
        ];
        for (toml, needle) in rows {
            let problems = problems_of(toml);
            assert!(problems.iter().any(|p| p.contains(needle)), "{toml}: {problems:?}");
        }
        assert!(problems_of("[backup]\nenabled = true\nto = \"/b\"\nevery = \"6h\"").is_empty());
    }

    #[test]
    fn backup_sink_must_be_disjoint_from_storage() {
        let s3 = |sink: &str| {
            format!(
                "[storage]\nbackend = \"s3\"\n[storage.s3]\nbucket = \"b\"\nprefix = \"oc\"\n\
                 [backup.sink]\nbackend = \"s3\"\n[backup.sink.s3]\n{sink}"
            )
        };
        let overlap = |toml: String| problems_of(&toml).iter().any(|p| p.contains("[backup.sink]"));
        assert!(overlap(s3("bucket = \"b\"\nprefix = \"oc\"")), "the same keyspace");
        assert!(overlap(s3("bucket = \"b\"\nprefix = \"oc/snapshots\"")), "inside it");
        assert!(overlap(s3("bucket = \"b\"")), "around it");
        assert!(!overlap(s3("bucket = \"b\"\nprefix = \"oc-backups\"")), "a sibling prefix");
        assert!(!overlap(s3("bucket = \"b\"\nprefix = \"oc\"\nendpoint = \"https://dr.example\"")), "another endpoint");
        let fs = |path: &str| format!("[server]\nstorage_path = \"/data/storage\"\n[backup.sink]\npath = \"{path}\"");
        assert!(overlap(fs("/data/storage/backups")));
        assert!(!overlap(fs("/data/storage-backups")));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = problems_of("[server]\nlease_wait = \"1s\"\nshutdown_grace = \"0s\"\nendpoint_drain = \"x\"");
        assert_eq!(problems.len(), 3, "{problems:?}");
    }
}
