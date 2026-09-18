//! Driven adapters: the detail behind a port.
//!
//! Only the composition root — `src/server.rs` and `src/main.rs` — may import
//! from here; every other module names a port. `scripts/boundary.sh` counts
//! the rule.

pub mod events;
pub mod fs;
pub mod sqlite;
pub mod system;
