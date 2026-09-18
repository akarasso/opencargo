//! Registry semantics: types and pure functions that would be true of a
//! registry written in any language. Nothing here performs I/O, and nothing
//! here names a transport, a driver or a storage format.

pub mod error;
pub mod kinds;
pub mod names;
pub mod repository;
pub mod webhook;

pub use error::{Action, DomainError, Resource};
pub use kinds::{Format, RepoKind, Visibility};
pub use names::{
    validate_npm_read_name, validate_oci_tag, validate_package_name, validate_version,
};
pub use repository::{
    DistTag, Package, Pending, RepoConfig, RepoSpec, Repository, Version,
};
pub use webhook::{Subscription, Webhook};
