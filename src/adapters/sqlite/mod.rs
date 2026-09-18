//! The SQLite adapter: the only place the dialect and the driver are named.

use chrono::{DateTime, Utc};

pub mod migrate;
pub mod multipart;

/// This adapter's stored timestamp: UTC, second precision.
///
/// It is the format the 23 columns defaulting to `datetime('now')` are written
/// in, and two live predicates compare such columns lexicographically — an
/// adapter that wrote RFC 3339 instead would sort `'T'` above `' '` and
/// mis-evaluate every legacy row. Every statement here binds it, so no column
/// default ever fires and the row carries the caller's clock.
pub(crate) fn bind_ts(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}
