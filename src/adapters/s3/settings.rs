//! What the S3 adapter is built from: the `[storage.s3]` block resolved
//! against an allowlisted environment, and the TLS roots it trusts.

use std::time::Duration;

use crate::config::{normalize_prefix, parse_duration, S3Config};

/// The only variables the adapter reads. Nothing else of the environment,
/// no profile file and no instance metadata, reaches the client.
pub const ENV_ALLOWLIST: &[&str] = &[
    "OPENCARGO_S3_ACCESS_KEY_ID",
    "OPENCARGO_S3_SECRET_ACCESS_KEY",
    "OPENCARGO_S3_SESSION_TOKEN",
    "OPENCARGO_S3_ENDPOINT",
    "OPENCARGO_S3_BUCKET",
    "OPENCARGO_S3_REGION",
    "OPENCARGO_S3_PREFIX",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
];

#[derive(Clone)]
pub struct S3Settings {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub allow_http: bool,
    pub virtual_hosted_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub request_timeout: Duration,
    pub completion_timeout: Duration,
    pub part_size: usize,
    pub max_multipart_uploads: usize,
    pub exists_cache_entries: usize,
    /// `None` keeps the client's retry policy; tests that inject faults set 0.
    pub max_retries: Option<usize>,
}

impl std::fmt::Debug for S3Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S3Settings(..)")
    }
}

fn pick(env: &dyn Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .inspect(|name| debug_assert!(ENV_ALLOWLIST.contains(name)))
        .find_map(|name| env(name).filter(|v| !v.is_empty()))
}

impl S3Settings {
    /// The block, overridden by the allowlisted variables `env` answers.
    pub fn resolve(cfg: &S3Config, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let bucket = pick(env, &["OPENCARGO_S3_BUCKET"]).unwrap_or_else(|| cfg.bucket.clone());
        if bucket.trim().is_empty() {
            anyhow::bail!("[storage.s3] bucket is required");
        }
        let prefix = pick(env, &["OPENCARGO_S3_PREFIX"]).unwrap_or_else(|| cfg.prefix.clone());
        let access_key_id = pick(env, &["OPENCARGO_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"])
            .ok_or_else(|| anyhow::anyhow!("S3 credentials: set OPENCARGO_S3_ACCESS_KEY_ID"))?;
        let secret_access_key = pick(env, &["OPENCARGO_S3_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"])
            .ok_or_else(|| anyhow::anyhow!("S3 credentials: set OPENCARGO_S3_SECRET_ACCESS_KEY"))?;
        Ok(Self {
            endpoint: pick(env, &["OPENCARGO_S3_ENDPOINT"]).or_else(|| cfg.endpoint.clone()),
            region: pick(env, &["OPENCARGO_S3_REGION", "AWS_REGION"]).unwrap_or_else(|| cfg.region.clone()),
            bucket,
            prefix: normalize_prefix(&prefix)?,
            allow_http: cfg.allow_http,
            virtual_hosted_style: cfg.virtual_hosted_style,
            access_key_id,
            secret_access_key,
            session_token: pick(env, &["OPENCARGO_S3_SESSION_TOKEN", "AWS_SESSION_TOKEN"]),
            request_timeout: parse_duration(&cfg.request_timeout)?,
            completion_timeout: parse_duration(&cfg.completion_timeout)?,
            part_size: usize::try_from(cfg.part_size_mib.max(5) * 1024 * 1024)?,
            max_multipart_uploads: cfg.max_multipart_uploads.max(1),
            exists_cache_entries: cfg.exists_cache_entries,
            max_retries: None,
        })
    }

    /// The process environment, through the allowlist.
    pub fn from_process(cfg: &S3Config) -> anyhow::Result<Self> {
        Self::resolve(cfg, &|name| std::env::var(name).ok())
    }
}

/// The roots the client trusts: the compiled-in Mozilla set, never the
/// system store, so a host's trust configuration cannot widen it.
pub fn tls_roots() -> Vec<&'static [u8]> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| der.as_ref())
        .collect()
}
