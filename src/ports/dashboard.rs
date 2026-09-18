//! The dashboard's read model: one method per panel, deliberately not
//! general.
//!
//! Nothing here is an aggregate. A panel is a shape the web UI asks for —
//! four counters, ten recent versions, a page of packages — and the query
//! that answers it joins across tables no store owns. Widening these methods
//! into a general reporting port is how a read model becomes a second, worse
//! copy of the write side; the rule is that a new panel gets a new method.
//!
//! The visibility predicate the panels used to splice into their SQL is
//! [`Reach`], a value the caller decides and the adapter binds. Who may see
//! what is the application's answer; how a dialect expresses it is not.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::Package;
use crate::error::StoreError;

/// Which rows a panel may see, and there is no third arm: a caller either has
/// the run of the place or sees the public repositories.
///
/// Packages and repositories answer to different callers — the package
/// panels admit an admin, the repository ones any authenticated user — which
/// is why the caller picks per panel rather than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Everything,
    PublicOnly,
}

/// The counters of the stats panel, in one round trip because they are one
/// panel. The repository count is not among them: it answers to a different
/// caller, so it would need a second [`Reach`] here and lose the property
/// that a panel's method takes the reach of its panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub packages: i64,
    pub versions: i64,
    pub downloads: i64,
}

/// One line of the "recently published" panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentVersion {
    pub package: String,
    pub version: String,
    pub published_at: DateTime<Utc>,
}

/// Which packages the list panel asks for: its two filter boxes, its reach
/// and its page.
pub struct PackageFilter<'a> {
    pub reach: Reach,
    /// A repository name, matched exactly.
    pub repository: Option<&'a str>,
    /// A substring of the package name. Not a search — how it is matched is
    /// the adapter's business, and relevance is [`crate::ports::search`]'s.
    pub name_contains: Option<&'a str>,
    pub limit: i64,
    pub offset: i64,
}

/// One row of the list panel: the package, what it was last released as, and
/// how often it was fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSummary {
    pub name: String,
    pub description: Option<String>,
    /// Absent while a package carries no version, which a publish that
    /// created the row and then failed used to leave behind.
    pub latest_version: Option<String>,
    pub downloads: i64,
    pub updated_at: DateTime<Utc>,
}

/// One page of the list panel, with the total its pager needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackagePage {
    pub packages: Vec<PackageSummary>,
    pub total: i64,
}

/// One row of the version table of the detail panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSummary {
    pub version: String,
    pub size: i64,
    pub published_at: DateTime<Utc>,
}

/// A tag and the version it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedVersion {
    pub tag: String,
    pub version: String,
}

/// Everything the detail panel shows about one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDetail {
    pub package: Package,
    /// Newest first, which is the order the panel renders.
    pub versions: Vec<VersionSummary>,
    pub dist_tags: Vec<TaggedVersion>,
    pub total_downloads: i64,
}

#[async_trait]
pub trait DashboardRead: Send + Sync {
    /// The package, version and download counters of the stats panel.
    async fn totals(&self, reach: Reach) -> Result<Totals, StoreError>;

    /// The repository counter of the same panel, separate because any
    /// authenticated caller may have it while only an admin may have the
    /// three above.
    async fn repository_count(&self, reach: Reach) -> Result<i64, StoreError>;

    /// The most recently published versions, newest first.
    async fn recent_versions(
        &self,
        reach: Reach,
        limit: i64,
    ) -> Result<Vec<RecentVersion>, StoreError>;

    /// One page of the list panel, most recently updated first.
    async fn packages(&self, filter: &PackageFilter<'_>) -> Result<PackagePage, StoreError>;

    /// The detail panel, or `None` when no package of that name is within
    /// reach — which is what keeps a private package a 404 rather than a
    /// different status for a caller who may not see it.
    async fn package_detail(
        &self,
        name: &str,
        reach: Reach,
    ) -> Result<Option<PackageDetail>, StoreError>;

    /// What each of those packages was last released as, keyed by package.
    ///
    /// The search panel's rows come out of [`crate::ports::search`], which
    /// answers with packages and knows nothing of versions; this is the one
    /// thing it still needs, asked for in one call rather than per row. A
    /// package with no version is absent from the map.
    async fn latest_versions(
        &self,
        packages: &[i64],
    ) -> Result<HashMap<i64, String>, StoreError>;
}
