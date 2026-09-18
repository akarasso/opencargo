//! Registry semantics: types and pure functions that would be true of a
//! registry written in any language. Nothing here performs I/O, and nothing
//! here names a transport, a driver or a storage format.

pub mod error;

pub use error::{Action, DomainError, Resource};
