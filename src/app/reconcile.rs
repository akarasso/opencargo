//! `Reconciler`: repairs a format's own invariants from `RunCleanup`, which
//! iterates whatever the composition root registered and knows no format.
//! One unit of work per item, one result per item: a conflict or a failure
//! on one item is reported and the pass goes on.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    Repaired,
    Clean,
    /// Left as is on purpose, e.g. a conflict another writer resolved.
    Skipped(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub outcome: Reconciled,
}

#[async_trait]
pub trait Reconciler: Send + Sync {
    fn name(&self) -> &'static str;

    async fn pass(&self, now: DateTime<Utc>) -> Vec<Item>;
}
