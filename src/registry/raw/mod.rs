//! Raw: arbitrary paths under one `/raw/` mount, hosted, proxied or grouped.

pub mod auth_rules;
pub mod http;
pub mod leaves;
pub mod routes;
pub mod upstream;

use crate::domain::{DomainError, Format};
use crate::registry::rules::rules_of;

/// The path prefix every raw route lives under, and a name no repository
/// may take.
pub const MOUNT: &str = "raw";

/// The path a client asked for, held to the format's rules; it is the whole
/// name a raw repository knows.
pub fn admit(path: &str) -> Result<&str, DomainError> {
    rules_of(Format::Raw)?.validate(path)?;
    Ok(path)
}
