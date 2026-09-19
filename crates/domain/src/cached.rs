//! What a proxy repository has been seen serving: a package's identity, kept
//! so that a search answers for the packages this server actually served and
//! not only for the ones it hosts.
//!
//! A sighting is about a package, never about a version: the versions of a
//! proxied package are the upstream's state, answered by the cached document
//! itself, and copying them here would be a second inventory that nothing
//! reads and that is wrong the moment it is written.

use chrono::{DateTime, Utc};

use crate::kinds::Format;

/// A package a proxy member answered for: borrowed in, owned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting<'a> {
    pub repository_id: i64,
    pub format: Format,
    pub name: &'a str,
    /// Only the formats whose metadata document carries one.
    pub description: Option<&'a str>,
    /// The newest version the document named, a hint for the reader.
    pub latest_version: Option<&'a str>,
}

/// One remembered package, as a search reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPackage {
    pub repository_id: i64,
    pub format: Format,
    pub name: String,
    pub description: Option<String>,
    pub latest_version: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}
