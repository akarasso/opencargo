//! The wall clock, for the callers that cannot take `now` as an argument.
//!
//! Every store method that writes or compares a timestamp takes the caller's
//! `now`, so the clock is a port only where there is no caller to take it
//! from: a background loop that sleeps, and a credential whose expiry is
//! computed from the moment it is issued.

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
