//! Port 23 (A1 C1): every storage key a committed row references or a
//! protecting pin covers, as one streamed read model. The claim of port 22
//! evaluates the same predicate in its own transaction.

use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::Stream;

use crate::error::StoreError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Referenced {
    pub key: String,
    /// The key protects everything under it, as an upload session does.
    pub prefix: bool,
}

pub type ReferencedStream = Pin<Box<dyn Stream<Item = Result<Referenced, StoreError>> + Send>>;

pub trait ReferencedKeys: Send + Sync {
    /// Pins count while live or expired for less than `grace`.
    fn referenced(&self, grace: Duration, now: DateTime<Utc>) -> ReferencedStream;
}
