//! Maven's use cases: deposits into a hosted repository, and the versions
//! rows that make a visible unit known to the generic readers.

pub mod admin;
pub mod deposit;
pub mod reconcile;
pub mod rules;
pub mod versions;

#[cfg(test)]
mod tests;
