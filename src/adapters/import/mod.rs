//! The importer's adapters: the command line that composes a run, the
//! vendor sources, the per-format sinks that publish into a target
//! opencargo, and the one HTTP gate every source request goes through.

pub mod cli;
pub mod http;
pub mod target;
