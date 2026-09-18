//! MCP governance use cases: syncing a mirror, probing its remotes,
//! supervising both, publishing skills, and deciding about what is served.

pub mod decide;
pub mod probe;
pub mod skills;
pub mod supervisor;
pub mod sync;
