//! The routing rules an operator declares, and the monotonic version every
//! snapshot of them carries.
//!
//! Patterns cross this port as the administrator wrote them: compiling one
//! needs the format's `FormatRules`, which lives in the registry layer, so the
//! store never holds a compiled form it could not rebuild.
//!
//! The version is the whole reason this port has a method that returns a
//! number. A rule changes nothing a group's view is otherwise keyed by — not
//! the members, not the hosted stamps — so any memo of a merged answer keys on
//! it too, or it keeps serving the answer from before the rule (I9).

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{RouteEffect as Effect, Format};
use crate::error::StoreError;

/// One rule as it is stored: patterns and exceptions as written, targets as
/// repository incarnations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRule {
    pub name: String,
    pub format: Format,
    pub patterns: Vec<String>,
    pub except: Vec<String>,
    pub effect: Effect,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a create or an update writes.
pub struct NewRule<'a> {
    pub name: &'a str,
    pub format: Format,
    pub patterns: &'a [String],
    pub except: &'a [String],
    pub effect: &'a Effect,
}

#[async_trait]
pub trait RoutingRuleStore: Send + Sync {
    /// Every rule, in name order: the order a refusal is imputed in.
    async fn all(&self) -> Result<Vec<StoredRule>, StoreError>;

    async fn by_name(&self, name: &str) -> Result<Option<StoredRule>, StoreError>;

    /// `Conflict` if the name is taken.
    async fn create(
        &self,
        rule: &NewRule<'_>,
        now: DateTime<Utc>,
    ) -> Result<StoredRule, StoreError>;

    /// `NotFound` if no rule bears the name; the name itself never changes.
    async fn update(
        &self,
        rule: &NewRule<'_>,
        now: DateTime<Utc>,
    ) -> Result<StoredRule, StoreError>;

    async fn delete(&self, name: &str) -> Result<(), StoreError>;

    /// Strictly increasing, bumped by every write here and by nothing else.
    async fn version(&self) -> Result<u64, StoreError>;

    /// Write the rules a configuration file declares, **and only on an empty
    /// table**. Answers how many were written, so nothing is what a seeded
    /// deployment answers ever after.
    ///
    /// Not the by-name semantics of `RepositoryStore` and `WebhookStore`, and
    /// the difference is the point: by name, a rule an operator deliberately
    /// deleted would come back at every restart. The cost is that hardening a
    /// pattern in `config.toml` of a live deployment does nothing, which is
    /// why the caller compares the two and says so at startup.
    async fn ensure_seeded(
        &self,
        rules: &[NewRule<'_>],
        now: DateTime<Utc>,
    ) -> Result<usize, StoreError>;
}
