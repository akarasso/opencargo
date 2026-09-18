//! The trail an administrative action leaves, and who left it.
//!
//! Every admin use case ends the same way — one append, one admin-only
//! announcement — so it is written once here rather than nine times. A failed
//! append is logged and swallowed: losing the record of an action that has
//! already happened must not undo it.

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::domain::{Audience, DomainEvent};
use crate::ports::audit::{AuditStore, NewAuditEntry};
use crate::ports::events::Events;

/// Who asked. `admin` is the caller's own standing, which some use cases
/// weigh again on top of what the route already required of them.
#[derive(Clone, Copy)]
pub struct Actor<'a> {
    /// `None` for a static token, which has no user row behind it.
    pub user_id: Option<i64>,
    pub username: &'a str,
    pub admin: bool,
}

pub async fn record(
    audit: &dyn AuditStore,
    events: &dyn Events,
    by: &Actor<'_>,
    action: &str,
    target: Option<&str>,
    now: DateTime<Utc>,
) {
    let entry = NewAuditEntry {
        user_id: by.user_id,
        username: Some(by.username),
        action,
        target,
        repository: None,
        ip: None,
        user_agent: None,
        details_json: None,
    };
    if let Err(e) = audit.append(&entry, now).await {
        warn!(error = %e, action, "failed to write audit log entry");
    }

    events.emit(
        DomainEvent::AuditEntry {
            username: by.username.to_string(),
            action: action.to_string(),
            target: target.map(str::to_string),
        },
        Audience::Admin,
    );
}
