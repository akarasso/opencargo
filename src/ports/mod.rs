//! The ports: what the layers above depend on, free of any technology.
//!
//! A port names a capability and its vocabulary of refusals; the adapter
//! behind it is chosen by the composition root and named nowhere else.

pub mod audit;
pub mod clock;
pub mod dashboard;
pub mod deps;
pub mod events;
pub mod ids;
pub mod multipart;
pub mod oci;
pub mod packages;
pub mod permissions;
pub mod policy;
pub mod proxy_cache;
pub mod pypi;
pub mod reclaim;
pub mod referenced;
pub mod repositories;
pub mod search;
pub mod secrets;
pub mod signing;
pub mod tokens;
pub mod users;
pub mod vulns;
pub mod webhooks;
