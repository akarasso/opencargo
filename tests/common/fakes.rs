//! In-memory doubles for the ports, shared by the lib's unit tests (through
//! `src/testing.rs`) and by every integration test that declares
//! `mod common;`. Everything here spells the crate `opencargo::`, like the
//! rest of `tests/common/`, which is what lets one file compile on both
//! sides.
//!
//! The `allow` is this file's own: `tests/common/mod.rs`'s inner attribute
//! does not reach it once the lib includes it under a different parent, and
//! CI runs clippy over the test targets with `-D warnings`.
#![allow(dead_code)]
