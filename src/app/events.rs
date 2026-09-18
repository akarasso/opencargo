//! Announcing what happened to the audience it is for.
//!
//! The audience of a package event is the application's decision and not the
//! bus's, because deciding it needs the repository row: an adapter with no
//! store would have to default, and defaulting `package.published` to
//! `Public` publishes private package names to anonymous subscribers.

use std::sync::Arc;

use tracing::warn;

use crate::domain::{announce, DomainEvent, Visibility};
use crate::ports::events::Events;
use crate::ports::repositories::RepositoryStore;

pub struct Announce {
    events: Arc<dyn Events>,
    repos: Arc<dyn RepositoryStore>,
}

impl Announce {
    pub fn new(events: Arc<dyn Events>, repos: Arc<dyn RepositoryStore>) -> Self {
        Self { events, repos }
    }

    /// Emit under the audience the repository's visibility dictates. A
    /// repository that cannot be read counts as private: the quiet answer is
    /// the safe one.
    pub async fn package_event(&self, event: DomainEvent, repository: &str) {
        let vis = match self.repos.by_name(repository).await {
            Ok(Some(repo)) => repo.visibility,
            Ok(None) => Visibility::Private,
            Err(e) => {
                warn!(repository, error = %e, "event audience decided as private: repository unreadable");
                Visibility::Private
            }
        };
        for (event, to) in announce(event, repository, vis) {
            self.events.emit(event, to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Audience, Format, PackageRelease, RepoKind, RepoSpec};
    use crate::error::StoreError;
    use crate::ports::events::Received;
    use crate::testing::fakes::{FakeDb, PortId};
    use chrono::Utc;

    fn release() -> DomainEvent {
        DomainEvent::PackagePublished(PackageRelease {
            package: "@sec/hidden".to_string(),
            version: "1.0.0".to_string(),
            repository: "npm-secret".to_string(),
            format: Format::Npm,
            published_by: "alice".to_string(),
        })
    }

    async fn announced(visibility: Option<Visibility>) -> Vec<(&'static str, Audience)> {
        let db = FakeDb::new();
        seed(&db, visibility).await;
        fan_out(&db).await
    }

    async fn seed(db: &FakeDb, visibility: Option<Visibility>) {
        let Some(vis) = visibility else { return };
        db.repositories()
            .create(
                &RepoSpec {
                    name: "npm-secret",
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility: vis,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
    }

    /// The events one publish puts on the bus, with the audience each got.
    async fn fan_out(db: &FakeDb) -> Vec<(&'static str, Audience)> {
        let bus = crate::server::event_bus();
        let mut sub = bus.subscribe();
        Announce::new(bus.clone(), db.repositories())
            .package_event(release(), "npm-secret")
            .await;

        let mut seen = Vec::new();
        while let Ok(Received::Event(e)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await
        {
            seen.push((e.event.kind(), e.audience));
        }
        seen
    }

    #[tokio::test]
    async fn a_public_repository_is_announced_to_everyone() {
        assert_eq!(
            announced(Some(Visibility::Public)).await,
            vec![("package.published", Audience::Public)]
        );
    }

    fn private_fan_out() -> Vec<(&'static str, Audience)> {
        vec![
            ("package.published", Audience::Admin),
            ("registry.changed", Audience::Authenticated),
        ]
    }

    /// The disclosure the store lookup exists to prevent, and the one a
    /// missing row must not reintroduce: no repository, no public announce.
    #[tokio::test]
    async fn a_private_or_unknown_repository_names_the_package_only_to_admins() {
        for visibility in [Some(Visibility::Private), None] {
            assert_eq!(announced(visibility).await, private_fan_out(), "{visibility:?}");
        }
    }

    /// The same fan-out when the store cannot answer at all: a failing
    /// lookup is not a reason to announce a package name to everyone, and a
    /// public repository whose row is unreadable is treated as private.
    #[tokio::test]
    async fn an_unreadable_repository_is_announced_as_private() {
        let db = FakeDb::new();
        seed(&db, Some(Visibility::Public)).await;
        db.fail_next(PortId::Repositories, StoreError::Other("disk".into()));

        assert_eq!(fan_out(&db).await, private_fan_out());
    }
}
