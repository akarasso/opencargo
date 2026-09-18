//! The dependency graph a publish records: what a version needs, and who
//! needs a name.
//!
//! Three methods, not five. The two the free-function DAL also carried —
//! every dependency of a package, and the dependents of one version — have no
//! caller in this tree; a port method with no caller and one implementation
//! is ceremony, and they are added back when something asks for them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;

/// One edge to record, as a publish reads it out of the metadata it just
/// stored.
pub struct NewDependency<'a> {
    pub package: i64,
    pub version: i64,
    pub name: &'a str,
    pub requirement: &'a str,
    /// The format's own word for the kind of edge: npm's `runtime`/`dev`/
    /// `peer`/`optional`, cargo's `normal`/`dev`/`build`.
    pub kind: &'a str,
}

/// One edge out of a version.
pub struct Dependency {
    pub name: String,
    pub requirement: String,
    pub kind: String,
}

/// One package-version that depends on the name asked about.
pub struct Dependent {
    pub name: String,
    pub version: String,
}

#[async_trait]
pub trait DependencyStore: Send + Sync {
    /// `now` fills `created_at`, so no column default ever fires.
    async fn record(
        &self,
        dep: &NewDependency<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// The edges out of one version, in the order they were recorded.
    async fn of_version(&self, version: i64) -> Result<Vec<Dependency>, StoreError>;

    /// Who depends on `name`. `public_only` is the visibility gate the
    /// dependency screens apply to anyone but an admin: a private
    /// repository's packages are not part of anyone else's graph.
    async fn dependents(
        &self,
        name: &str,
        public_only: bool,
    ) -> Result<Vec<Dependent>, StoreError>;
}
