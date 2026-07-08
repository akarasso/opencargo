//! Real-time event bus.
//!
//! A single in-process `tokio::sync::broadcast` channel that fans domain
//! events out to every connected WebSocket client (`src/api/ws.rs`). Events
//! carry a visibility level so a connection only ever receives what its auth
//! level allows:
//!
//! - `Public`        — safe for anonymous readers (public-repo activity)
//! - `Authenticated` — any logged-in user (private-repo *hints*, no payload
//!   details that could leak package names across teams)
//! - `Admin`         — admin role only (audit trail, user/token/webhook CRUD)
//!
//! Emission is best-effort: a send with no subscribers is not an error, and
//! the bus must never make the underlying operation fail.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

/// Who is allowed to receive an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Visibility {
    Public = 0,
    Authenticated = 1,
    Admin = 2,
}

/// A single real-time event, broadcast to WebSocket subscribers.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    /// Dotted event name, e.g. `package.published`, `audit.entry`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event payload (shape depends on `event_type`).
    pub data: serde_json::Value,
    /// RFC 3339 emission timestamp.
    pub ts: String,
    #[serde(skip)]
    pub visibility: Visibility,
}

#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        // 256 pending events per subscriber before it lags; a lagged client
        // gets a `resync` marker from the WS layer and refetches via REST.
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    /// Broadcast an event. No-op when nobody is listening.
    pub fn emit(&self, event_type: &str, visibility: Visibility, data: serde_json::Value) {
        let event = Arc::new(Event {
            event_type: event_type.to_string(),
            data,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            visibility,
        });
        // Err only means "no active subscribers" — fine.
        let _ = self.tx.send(event);
    }
}
