//! One sink per opencargo format: each publishes over the target's own
//! protocol through the shared lane, and reads the source only through the
//! gate.

use reqwest::Url;

pub mod cargo;
pub mod go;
pub mod npm;

/// `{base}{name}` with a scoped npm name's `/` kept in one path segment.
pub fn npm_url(base: &Url, name: &str) -> Url {
    let encoded = name.replacen('/', "%2f", 1);
    base.join(&encoded).unwrap_or_else(|_| base.clone())
}

/// The basename npm keys a version's tarball by.
pub fn tarball_name(package: &str, version: &str) -> String {
    let short = package.split_once('/').map_or(package, |(_, n)| n);
    format!("{short}-{version}.tgz")
}
