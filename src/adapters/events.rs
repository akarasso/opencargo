//! The in-process bus: one `tokio::sync::broadcast` channel, every
//! subscriber receiving every event and filtering by audience itself.
//!
//! `ha-options.md` names this as the thing that breaks visibly under two
//! instances; a `LISTEN`/`NOTIFY` or Redis implementation of the same port is
//! where that repair goes.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::domain::{Audience, DomainEvent};
use crate::ports::events::{Emitted, Events, Received, Subscriber};

/// Pending events per subscriber before it lags. A lagged client is told to
/// resync rather than trusting the stream.
const DEPTH: usize = 256;

#[derive(Debug)]
pub struct BroadcastEvents {
    tx: broadcast::Sender<Arc<Emitted>>,
}

impl Default for BroadcastEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl BroadcastEvents {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(DEPTH);
        Self { tx }
    }
}

impl Events for BroadcastEvents {
    fn emit(&self, event: DomainEvent, to: Audience) {
        let emitted = Arc::new(Emitted {
            event,
            audience: to,
            at: chrono::Utc::now(),
        });
        // Err only means "no active subscribers", which is not a failure.
        let _ = self.tx.send(emitted);
    }

    fn subscribe(&self) -> Box<dyn Subscriber> {
        Box::new(BroadcastSubscriber {
            rx: self.tx.subscribe(),
        })
    }
}

struct BroadcastSubscriber {
    rx: broadcast::Receiver<Arc<Emitted>>,
}

#[async_trait]
impl Subscriber for BroadcastSubscriber {
    async fn recv(&mut self) -> Received {
        match self.rx.recv().await {
            Ok(emitted) => Received::Event(emitted),
            Err(broadcast::error::RecvError::Lagged(_)) => Received::Lagged,
            Err(broadcast::error::RecvError::Closed) => Received::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Visibility;

    #[tokio::test]
    async fn an_emitted_event_reaches_every_subscriber_with_its_audience() {
        let bus = BroadcastEvents::new();
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();

        bus.emit(DomainEvent::RepositoriesChanged, Audience::Public);

        for sub in [&mut first, &mut second] {
            match sub.recv().await {
                Received::Event(e) => {
                    assert_eq!(e.event, DomainEvent::RepositoriesChanged);
                    assert_eq!(e.audience, Audience::Public);
                }
                other => panic!("expected an event, got {other:?}"),
            }
        }
    }

    /// The fan-out the application decides, seen from the bus: two events,
    /// and only the second one is safe for a signed-in non-admin.
    #[tokio::test]
    async fn the_private_fan_out_arrives_as_two_events() {
        let bus = BroadcastEvents::new();
        let mut sub = bus.subscribe();

        for (event, to) in crate::domain::announce(
            DomainEvent::PermissionsChanged {
                username: "alice".to_string(),
            },
            "npm-secret",
            Visibility::Private,
        ) {
            bus.emit(event, to);
        }

        let mut seen = Vec::new();
        for _ in 0..2 {
            match sub.recv().await {
                Received::Event(e) => seen.push((e.event.kind(), e.audience)),
                other => panic!("expected an event, got {other:?}"),
            }
        }
        assert_eq!(
            seen,
            vec![
                ("permissions.changed", Audience::Admin),
                ("registry.changed", Audience::Authenticated),
            ]
        );
    }

    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_told_it_lagged() {
        let bus = BroadcastEvents::new();
        let mut sub = bus.subscribe();
        for _ in 0..DEPTH + 1 {
            bus.emit(DomainEvent::RepositoriesChanged, Audience::Public);
        }
        assert!(matches!(sub.recv().await, Received::Lagged));
    }
}
