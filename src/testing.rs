//! The one file in the tree carrying a relative path to `tests/`.
//!
//! Fakes have to be visible from both sides: `tests/common/` compiles into
//! every integration test, and the lib's own unit tests need the same types.
//! `#[cfg(any(test, feature = "fakes"))]` cannot do it -- an integration test
//! links the library built without `cfg(test)`, and a crate cannot enable its
//! own feature for its own test targets -- so the file lives under `tests/`
//! and is pulled into the lib here, exactly once. Unit tests then say
//! `use crate::testing::fakes::..` and never spell a `#[path]` of their own,
//! whose `../` depth would differ per module.

#[cfg(test)]
#[path = "../tests/common/fakes.rs"]
pub mod fakes;

/// The proxy-engine fixture: a temp database, a fake upstream and a storage
/// root. It lives here rather than under `src/proxy/` because four other
/// modules build their engine and their pool out of it, and because the proxy
/// itself no longer knows what a pool is.
#[cfg(test)]
pub mod fixture;

/// A resolver context over those fakes: what `Cx` collapses into once it
/// holds ports instead of the whole application state.
#[cfg(test)]
pub mod resolver;

/// An in-memory storage backend with a delete log and injected faults.
#[cfg(test)]
pub mod storage;

/// An in-process S3 endpoint with injectable faults, for the S3 adapter.
#[cfg(test)]
pub mod fake_s3;

/// The multipart ledger in memory.
#[cfg(test)]
pub mod ledger;
