//! What becomes of a refused member.
//!
//! A refusal happens at the cadence of a `pnpm install`, and the audit table
//! is the log of administrative actions, kept without retention: writing one
//! line per refusal would drown it. So two levels. The **first sighting** of a
//! triplet in a window is an audit event carrying the actor and every rule
//! that refused; everything after it is a counter and a coalesced
//! announcement.
//!
//! The deduplication key holds no rule name. With one in it, renaming a rule
//! — or adding one that sorts before it — would fire the same alarm again or
//! re-attribute it, which is exactly the kind of noise that gets an alarm
//! switched off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::domain::{Audience, DomainEvent};
use crate::ports::audit::{AuditStore, NewAuditEntry};
use crate::ports::events::Events;
use crate::registry::routing::{RefusalRecorder, Refused};

/// `(ident_key, addressed repository, refused member)`.
type Seen = (String, String, String);

pub struct RefusalLog {
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
    window: Duration,
    /// Bounded: a flood of distinct names must not become a flood of memory.
    budget: usize,
    seen: Mutex<HashMap<Seen, Instant>>,
}

impl RefusalLog {
    pub fn new(audit: Arc<dyn AuditStore>, events: Arc<dyn Events>, window: Duration) -> Self {
        Self {
            audit,
            events,
            window,
            budget: 10_000,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this is the first sighting of the triplet in the window, and
    /// the only place the table is written.
    fn first(&self, key: Seen) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, at| now.duration_since(*at) < self.window);
        if seen.len() >= self.budget {
            // Past the budget every refusal is a first sighting again. Losing
            // the deduplication is noisy; losing the alarm is not an option.
            seen.clear();
        }
        seen.insert(key, now).is_none()
    }
}

impl RefusalRecorder for RefusalLog {
    fn records(&self) -> bool {
        true
    }

    fn refused(&self, refusal: Refused<'_>) {
        metrics::counter!(
            "opencargo_routing_refusals_total",
            "repository" => refusal.addressed.to_string(),
            "member" => refusal.member.to_string(),
            "format" => refusal.format.as_str(),
            "rule" => refusal.rules.first().cloned().unwrap_or_else(|| "stale".to_string()),
        )
        .increment(1);

        let key = (
            refusal.ident_key.to_string(),
            refusal.addressed.to_string(),
            refusal.member.to_string(),
        );
        if !self.first(key) {
            return;
        }

        let details = serde_json::json!({
            "member": refusal.member,
            "format": refusal.format.as_str(),
            "ident_key": refusal.ident_key,
            "rules": refusal.rules,
            "stale": refusal.stale,
            "actor_kind": refusal.actor_kind,
        })
        .to_string();
        let entry = Recorded {
            user_id: refusal.user_id,
            action: if refusal.stale {
                "routing.refused.stale"
            } else {
                "routing.refused"
            },
            target: refusal.addressed.to_string(),
            details,
        };
        let audit = self.audit.clone();
        let events = self.events.clone();
        // Off the read path: a refusal has already happened, and the client's
        // answer does not wait on the record of it.
        tokio::spawn(async move {
            let written = NewAuditEntry {
                user_id: entry.user_id,
                username: None,
                action: entry.action,
                target: Some(&entry.target),
                repository: Some(&entry.target),
                ip: None,
                user_agent: None,
                details_json: Some(&entry.details),
            };
            if let Err(e) = audit.append(&written, chrono::Utc::now()).await {
                tracing::warn!(error = %e, "failed to record a routing refusal");
            }
            events.emit(
                DomainEvent::AuditEntry {
                    username: String::new(),
                    action: entry.action.to_string(),
                    target: Some(entry.target),
                },
                Audience::Admin,
            );
        });
    }
}

/// The owned copy the spawned write needs; `Refused` borrows the walk.
struct Recorded {
    user_id: Option<i64>,
    action: &'static str,
    target: String,
    details: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Format;
    use crate::testing::fakes::FakeDb;

    fn log(db: &FakeDb, window: Duration) -> RefusalLog {
        RefusalLog::new(db.audit(), crate::server::event_bus(), window)
    }

    /// The audit write is spawned, so the assertion waits for it on a
    /// deadline rather than on a margin.
    async fn rows(db: &FakeDb, expected: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let actions: Vec<String> = db
                .audit_rows()
                .into_iter()
                .map(|(_, action, _)| action)
                .collect();
            if actions.len() >= expected || tokio::time::Instant::now() >= deadline {
                return actions;
            }
            tokio::task::yield_now().await;
        }
    }

    fn refusal<'a>(ident: &'a str, member: &'a str, rules: &'a [String]) -> Refused<'a> {
        Refused {
            format: Format::Npm,
            addressed: "all",
            member,
            ident_key: ident,
            rules,
            stale: false,
            user_id: None,
            actor_kind: "anonymous",
        }
    }

    /// T9: one event for a hundred refusals of one triplet, and renaming the
    /// rule or adding one before it does not fire it again.
    #[tokio::test]
    async fn the_first_sighting_is_the_event_and_the_rule_is_not_in_the_key() {
        let db = FakeDb::new();
        let log = log(&db, Duration::from_secs(600));
        let one = vec!["acme-internal".to_string()];
        for _ in 0..100 {
            log.refused(refusal("@acme/foo", "public", &one));
        }
        let two = vec!["aaa-first".to_string(), "acme-internal".to_string()];
        log.refused(refusal("@acme/foo", "public", &two));
        let renamed = vec!["zzz-renamed".to_string()];
        log.refused(refusal("@acme/foo", "public", &renamed));

        log.refused(refusal("@acme/other", "public", &one));
        log.refused(refusal("@acme/foo", "second-proxy", &one));

        let actions = rows(&db, 3).await;
        assert_eq!(
            actions,
            vec!["routing.refused".to_string(); 3],
            "one per triplet: the name, the repository addressed and the member"
        );
    }

    /// A window that has passed lets the alarm fire again, which is what
    /// makes probing for the patterns noisy rather than free (D8bis).
    #[tokio::test]
    async fn a_passed_window_fires_again() {
        let db = FakeDb::new();
        let log = log(&db, Duration::ZERO);
        let rules = vec!["acme-internal".to_string()];
        log.refused(refusal("@acme/foo", "public", &rules));
        log.refused(refusal("@acme/foo", "public", &rules));
        assert_eq!(rows(&db, 2).await.len(), 2);
    }
}
