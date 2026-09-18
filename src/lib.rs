// `opencargo::` resolves inside the lib too, so `tests/common/fakes.rs` --
// which is compiled both as part of an integration test and, through
// `src/testing.rs`, as part of this crate -- can spell one path either way.
extern crate self as opencargo;

pub mod adapters;
pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod events;
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
