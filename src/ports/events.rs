//! The real-time bus: what the application announces, and what a live
//! connection waits on.
//!
//! Emission never fails and never blocks — the bus must not be able to make
//! the operation that triggered it fail — so `emit` returns nothing.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{Audience, DomainEvent};

/// One event as it left the application, stamped by the bus on the way out.
#[derive(Debug)]
pub struct Emitted {
    pub event: DomainEvent,
    pub audience: Audience,
    pub at: DateTime<Utc>,
}

/// What a subscriber's next wait yields.
#[derive(Debug)]
pub enum Received {
    Event(Arc<Emitted>),
    /// The subscriber fell behind and events were dropped for it.
    Lagged,
    Closed,
}

#[async_trait]
pub trait Subscriber: Send {
    async fn recv(&mut self) -> Received;
}

pub trait Events: Send + Sync {
    fn emit(&self, event: DomainEvent, to: Audience);
    fn subscribe(&self) -> Box<dyn Subscriber>;
}
