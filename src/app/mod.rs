//! The use cases: one per protocol operation and admin action.
//!
//! A use case holds the ports it needs and nothing else — never the
//! application state, never a handler's request. It owns the order of the
//! calls, what is atomic and what is recorded; the driving adapter above it
//! parses, calls one of these, and encodes the answer.

pub mod promote;
pub mod publish;
pub mod webhooks;
