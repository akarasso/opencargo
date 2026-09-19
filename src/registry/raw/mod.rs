//! Raw: arbitrary paths under one `/raw/` mount, hosted, proxied or grouped.

pub mod auth_rules;
pub mod http;
pub mod leaves;
pub mod routes;
pub mod upstream;

use crate::domain::{DomainError, Format};
use crate::registry::rules::{rules_of, RawPathBound};

/// The path prefix every raw route lives under, and a name no repository
/// may take.
pub const MOUNT: &str = "raw";

/// The path a client asked for, held to the format's rules and to `bound`,
/// what the store this server runs on can key; it is the whole name a raw
/// repository knows. Refused here, before any body is read, and named as
/// the path rather than as a key the caller never wrote.
pub fn admit<'a>(path: &'a str, bound: &RawPathBound) -> Result<&'a str, DomainError> {
    rules_of(Format::Raw)?.validate(path)?;
    if path.len() > bound.path {
        return Err(DomainError::InvalidName(format!(
            "raw path over {} bytes: '{path}'",
            bound.path
        )));
    }
    if path.split('/').any(|s| s.len() > bound.segment) {
        return Err(DomainError::InvalidName(format!(
            "raw path segment over {} bytes: '{path}'",
            bound.segment
        )));
    }
    Ok(path)
}
