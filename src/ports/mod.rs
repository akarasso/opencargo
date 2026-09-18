//! The ports: what the layers above depend on, free of any technology.
//!
//! A port names a capability and its vocabulary of refusals; the adapter
//! behind it is chosen by the composition root and named nowhere else.

pub mod multipart;
pub mod proxy_cache;
pub mod webhooks;
