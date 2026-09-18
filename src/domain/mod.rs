//! Registry semantics: types and pure functions that would be true of a
//! registry written in any language. Nothing here performs I/O, and nothing
//! here names a transport, a driver or a storage format.

pub mod error;
pub mod kinds;
pub mod names;
pub mod permission;
pub mod repository;
pub mod user;
pub mod webhook;

pub use error::{Action, DomainError, Resource};
pub use kinds::{Format, RepoKind, Visibility};
pub use names::{
    validate_npm_read_name, validate_oci_tag, validate_package_name, validate_version,
};
pub use permission::{allows, can_admin, effective_rights, RepoAction, Rights, RightsSource};
pub use repository::{DistTag, Package, RepoConfig, Repository, Version};
pub use user::{ApiToken, User};
pub use webhook::{Subscription, Webhook};
