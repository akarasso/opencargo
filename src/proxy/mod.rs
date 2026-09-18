use std::net::{IpAddr, ToSocketAddrs};

use serde_json::Value;

use crate::error::{AppError, AppResult};

pub mod auth;
pub mod engine;
pub mod purge;
pub mod singleflight;
pub mod strategy;

pub use auth::{UpstreamAuth, UpstreamCreds};
pub use engine::{IntoPayload, Payload, ProxyEngine, Timeouts, TtlConfig};
pub use strategy::UpstreamStrategy;

/// True for addresses the proxy must never reach. Names are resolved once
/// before the request, which does not cover DNS rebinding.
pub(crate) fn is_blocked_ip(ip: &IpAddr) -> bool {
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

/// Validate an admin-chosen upstream URL: http(s) only, and no link-local or
/// unspecified IP literal. Loopback and RFC 1918 stay allowed for local
/// mirrors; redirect hops and upstream-chosen URLs are held to `is_blocked_ip`.
pub fn validate_upstream_url(url: &str) -> Result<(), AppError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| AppError::BadRequest(format!("invalid upstream URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest(
            "upstream URL scheme must be http or https".to_string(),
        ));
    }
    let literal = parsed
        .host_str()
        .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok());
    let refused = match literal {
        Some(std::net::IpAddr::V4(v4)) => v4.is_link_local() || v4.is_unspecified(),
        Some(std::net::IpAddr::V6(v6)) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80 || v6.is_unspecified()
        }
        None => false,
    };
    if refused {
        return Err(AppError::BadRequest(
            "upstream URL must not point at a link-local or unspecified address".to_string(),
        ));
    }
    Ok(())
}

fn ip_literal(url: &reqwest::Url) -> Option<IpAddr> {
    url.host_str()?.trim_matches(['[', ']']).parse().ok()
}

fn host_port(url: &reqwest::Url) -> AppResult<(&str, u16)> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::BadGateway(format!("{url} has no host")))?;
    Ok((host, url.port_or_known_default().unwrap_or(443)))
}

fn first_blocked(addrs: impl IntoIterator<Item = IpAddr>) -> Option<IpAddr> {
    addrs.into_iter().find(is_blocked_ip)
}

pub(crate) fn same_endpoint(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// Refuse an upstream-chosen URL whose host is, or resolves to, an address
/// the proxy must never reach.
pub(crate) async fn refuse_blocked_host(url: &reqwest::Url) -> AppResult<()> {
    let blocked = match ip_literal(url) {
        Some(ip) => first_blocked([ip]),
        None => {
            let (host, port) = host_port(url)?;
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| AppError::BadGateway(format!("{url} does not resolve: {e}")))?;
            first_blocked(addrs.map(|a| a.ip()))
        }
    };
    match blocked {
        Some(ip) => Err(AppError::BadGateway(format!(
            "{url} points at a private address ({ip})"
        ))),
        None => Ok(()),
    }
}

// A redirect policy is synchronous; the OS resolver is consulted in place,
// once per cross-origin hop.
fn blocked_hop(url: &reqwest::Url) -> Option<IpAddr> {
    if let Some(ip) = ip_literal(url) {
        return first_blocked([ip]);
    }
    let addrs = host_port(url).ok()?.to_socket_addrs().ok()?;
    first_blocked(addrs.map(|a| a.ip()))
}

/// SSRF hardening: cap redirects (was 10 by default), follow hops on the
/// origin the admin chose, and hold every other hop to `is_blocked_ip`.
pub(crate) fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        let origin = attempt.previous().first();
        if origin.is_some_and(|o| same_endpoint(o, attempt.url())) {
            return attempt.follow();
        }
        match blocked_hop(attempt.url()) {
            Some(_) => attempt.stop(),
            None => attempt.follow(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn blocked_hosts_are_refused_as_literals_and_as_names() {
        for private in [
            "http://127.0.0.1:1/x",
            "http://10.0.0.5/x",
            "http://[::1]/x",
            "http://localhost:1/x",
        ] {
            let err = refuse_blocked_host(&url(private)).await.unwrap_err();
            assert!(
                matches!(err, AppError::BadGateway(ref m) if m.contains("private address")),
                "{private}: {err}"
            );
            assert!(blocked_hop(&url(private)).is_some(), "{private}");
        }
        assert!(refuse_blocked_host(&url("http://93.184.216.34/x"))
            .await
            .is_ok());
        assert!(blocked_hop(&url("http://93.184.216.34/x")).is_none());
        assert!(same_endpoint(&url("http://h:80/a"), &url("http://h/b")));
        assert!(!same_endpoint(&url("http://h:81/a"), &url("http://h/b")));
    }
}
