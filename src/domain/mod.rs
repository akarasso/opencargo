//! Registry semantics: types and pure functions that would be true of a
//! registry written in any language. Nothing here performs I/O, and nothing
//! here names a transport, a driver or a storage format.

pub mod error;
pub mod events;
pub mod kinds;
pub mod names;
pub mod permission;
pub mod policy;
pub mod proxy;
pub mod repository;
pub mod resolve;
pub mod user;
pub mod vulns;
pub mod webhook;

pub use error::{Action, DomainError, Resource};
pub use events::{
    announce, Audience, DomainEvent, PackagePromotion, PackageRelease, ResolutionCounts,
};
pub use kinds::{Format, RepoKind, Visibility};
pub use names::{
    validate_npm_read_name, validate_oci_tag, validate_package_name, validate_version,
};
pub use permission::{allows, can_admin, effective_rights, RepoAction, Rights, RightsSource};
pub use policy::{RuleVerdict, Verdict};
pub use proxy::{
    CacheEntry, CacheEntryId, CachePolicy, Classified, NewEntry, RepoId, Transfer, Ttl, UrlSource,
};
pub use repository::{DistTag, Package, Pending, RepoConfig, RepoSpec, Repository, Version};
pub use resolve::{CacheRepo, Miss, Outcome, UrlRepo, Visit, Walk, MAX_GROUP_DEPTH};
pub use user::{ApiToken, User};
pub use vulns::{ScanResult, Severity, VulnDetail};
pub use webhook::{Subscription, Webhook};
