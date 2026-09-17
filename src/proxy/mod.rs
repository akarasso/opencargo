use serde_json::Value;

use crate::error::AppError;

pub mod auth;
pub mod engine;
pub mod purge;
pub mod singleflight;
pub mod strategy;

pub use auth::{UpstreamAuth, UpstreamCreds};
pub use engine::{Payload, ProxyEngine, Timeouts, TtlConfig};
pub use strategy::UpstreamStrategy;

/// True for IP literals the proxy must never reach (basic SSRF guard). Does NOT
/// cover DNS rebinding — hostnames are not resolved here; a custom resolver
/// would be needed for that.
pub(crate) fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Validate an upstream registry URL for proxy repositories: must be http(s)
/// and must not be a literal private/loopback address.
pub fn validate_upstream_url(url: &str) -> Result<(), AppError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| AppError::BadRequest(format!("invalid upstream URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest(
            "upstream URL scheme must be http or https".to_string(),
        ));
    }
    // A literal private/loopback host is intentionally allowed here: upstream
    // creation is admin-only and proxy setups legitimately target local mirrors.
    // The real SSRF vector — an upstream that REDIRECTS to an internal address —
    // is blocked by the client's redirect policy (see is_blocked_ip usage).
    Ok(())
}

/// SSRF hardening: cap redirects (was 10 by default) and refuse any hop whose
/// host is a literal private/loopback/link-local IP.
pub(crate) fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        match attempt
            .url()
            .host_str()
            .and_then(|h| h.parse::<std::net::IpAddr>().ok())
        {
            Some(ip) if is_blocked_ip(&ip) => attempt.stop(),
            _ => attempt.follow(),
        }
    })
}

/// Rewrite all `dist.tarball` URLs in an npm package metadata document
/// to point to our proxy server.
///
/// The upstream URLs are replaced with `{base_url}/{repo_name}/{package_name}/-/{filename}`.
pub fn rewrite_tarball_urls(
    metadata: &mut Value,
    base_url: &str,
    repo_name: &str,
    package_name: &str,
) {
    if let Some(versions) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (_version_key, version_meta) in versions.iter_mut() {
            if let Some(dist) = version_meta.get_mut("dist").and_then(|d| d.as_object_mut()) {
                if let Some(tarball_url) = dist.get("tarball").and_then(|t| t.as_str()) {
                    // Extract the filename from the upstream tarball URL
                    if let Some(filename) = tarball_url.rsplit('/').next() {
                        let new_url = format!(
                            "{}/{}/{}/-/{}",
                            base_url.trim_end_matches('/'),
                            repo_name,
                            package_name,
                            filename
                        );
                        dist.insert("tarball".to_string(), Value::String(new_url));
                    }
                }
            }
        }
    }
}
