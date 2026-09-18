//! Maven: hosted, proxy and group repositories under one `/maven/` mount.

pub mod auth_rules;
pub mod hosted;
pub mod http;
pub mod metadata;
pub mod path;
pub mod pom;
pub mod routes;
pub mod version;

/// The path prefix every Maven route lives under, and a name no repository
/// may take.
pub const MOUNT: &str = "maven";
