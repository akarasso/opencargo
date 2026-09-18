// `opencargo::` resolves inside the lib too, so `tests/common/fakes.rs` --
// which is compiled both as part of an integration test and, through
// `src/testing.rs`, as part of this crate -- can spell one path either way.
extern crate self as opencargo;

// The domain moved out to `crates/domain` and keeps its name here: one alias
// spares 168 call sites a rename that would say nothing the crate split does
// not already enforce.
pub use opencargo_domain as domain;

pub mod adapters;
pub mod api;
pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod policy;
pub mod ports;
pub mod proxy;
pub mod registry;
pub mod server;
pub mod storage;
pub mod telemetry;
pub mod testing;
pub mod web;
pub mod wire;
