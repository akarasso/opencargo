//! Administering a repository: creating one, changing one, removing one.
//!
//! Each owns the order its writes happen in and what happens outside them —
//! a proxy's cached files are dropped after the transaction that removes its
//! row, and before the one that repoints it at a different upstream, because
//! a store method touches nothing but the database and the cache rows are not
//! keyed by the upstream they came from.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::app::audit::{self, Actor};
use crate::app::repo_spec::validate_spec;
use crate::domain::{
    Audience, DomainEvent, RepoConfig, RepoKind, RepoSpec, Repository, Visibility,
};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::repositories::{RepoPatch, RepositoryStore};
use crate::proxy::engine::ProxyEngine;
use crate::proxy::purge::purge_repository;
use crate::registry::load_repo;

/// The trail an administrative change leaves, and the payload-free hint that
/// the repository list moved. The hint carries nothing because the anonymous
/// view of that list is already public and every client refetches it through
/// the REST API, which filters per caller.
async fn recorded(
    audit: &dyn AuditStore,
    events: &dyn Events,
    by: &Actor<'_>,
    action: &str,
    name: &str,
    now: DateTime<Utc>,
) {
    audit::record(audit, events, by, action, Some(name), now).await;
    events.emit(DomainEvent::RepositoriesChanged, Audience::Public);
}

/// Names of the groups listing `name` as a member.
async fn groups_containing(repos: &dyn RepositoryStore, name: &str) -> AppResult<Vec<String>> {
    let mut holders = Vec::new();
    for repo in repos.all().await? {
        if repo.kind()? == RepoKind::Group && repo.members().iter().any(|m| m == name) {
            holders.push(repo.name);
        }
    }
    Ok(holders)
}

pub struct CreateRepository {
    repos: Arc<dyn RepositoryStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl CreateRepository {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self {
            repos,
            audit,
            events,
        }
    }

    pub async fn run(
        &self,
        spec: &RepoSpec<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<Repository> {
        if self.repos.by_name(spec.name).await?.is_some() {
            return Err(AppError::Conflict(format!(
                "repository already exists: {}",
                spec.name
            )));
        }
        validate_spec(self.repos.as_ref(), spec, &[]).await?;

        let repo = self.repos.create(spec, now).await?;
        recorded(
            &*self.audit,
            &*self.events,
            by,
            "repo.create",
            &repo.name,
            now,
        )
        .await;
        Ok(repo)
    }
}

/// What a change asks for; `None` leaves a field as it stands. The empty
/// upstream the UI sends for "none" reaches here as `None`.
pub struct RepoUpdate<'a> {
    pub visibility: Option<Visibility>,
    pub upstream: Option<&'a str>,
    pub members: Option<&'a [String]>,
}

pub struct UpdateRepository {
    repos: Arc<dyn RepositoryStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
    proxy: ProxyEngine,
}

impl UpdateRepository {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
        proxy: ProxyEngine,
    ) -> Self {
        Self {
            repos,
            audit,
            events,
            proxy,
        }
    }

    /// The patch is merged onto the stored row and validated like a create.
    /// A new upstream purges the cache first: the rows are not keyed by it,
    /// so what they hold would otherwise be served as the new upstream's.
    pub async fn run(
        &self,
        name: &str,
        update: &RepoUpdate<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<Repository> {
        let repo = load_repo(self.repos.as_ref(), name).await?;
        let members = update
            .members
            .map_or_else(|| repo.members(), <[String]>::to_vec);
        let spec = RepoSpec {
            name: &repo.name,
            kind: repo.kind()?,
            format: repo.fmt()?,
            visibility: update.visibility.unwrap_or(repo.visibility),
            upstream: update.upstream.or(repo.upstream_url.as_deref()),
            members: &members,
        };
        validate_spec(self.repos.as_ref(), &spec, &[]).await?;
        if update.upstream.is_some_and(|u| Some(u) != repo.upstream_url.as_deref()) {
            purge_repository(&self.proxy, self.repos.as_ref(), &repo).await?;
        }

        let config = update
            .members
            .is_some()
            .then(|| RepoConfig::of_members(&members));
        let updated = self
            .repos
            .update(
                name,
                &RepoPatch {
                    visibility: update.visibility,
                    upstream: update.upstream,
                    config: config.as_ref(),
                },
                now,
            )
            .await?;

        recorded(&*self.audit, &*self.events, by, "repo.update", name, now).await;
        Ok(updated)
    }
}

pub struct DeleteRepository {
    repos: Arc<dyn RepositoryStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
    proxy: ProxyEngine,
}

impl DeleteRepository {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
        proxy: ProxyEngine,
    ) -> Self {
        Self {
            repos,
            audit,
            events,
            proxy,
        }
    }

    /// Refused while packages or a group membership remain. The cached files
    /// go after the commit and outside it: a store method touches nothing but
    /// the database, and a refused delete purges nothing. A group owns no
    /// cache — its members keep theirs.
    pub async fn run(&self, name: &str, by: &Actor<'_>, now: DateTime<Utc>) -> AppResult<()> {
        let repo = load_repo(self.repos.as_ref(), name).await?;
        let holders = groups_containing(self.repos.as_ref(), name).await?;
        if !holders.is_empty() {
            return Err(AppError::Conflict(format!(
                "repository '{name}' is a member of group(s) {}; remove it from them first",
                holders.join(", ")
            )));
        }

        let kind = repo.kind()?;
        self.repos
            .delete_empty(name)
            .await
            .map_err(|err| match err {
                StoreError::Conflict => AppError::Conflict(format!(
                    "repository '{name}' is not empty; delete its packages first"
                )),
                other => other.into(),
            })?;

        if kind == RepoKind::Proxy {
            purge_repository(&self.proxy, self.repos.as_ref(), &repo).await?;
        }

        recorded(&*self.audit, &*self.events, by, "repo.delete", name, now).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Format;
    use crate::testing::fakes::FakeDb;

    fn spec<'a>(name: &'a str, members: &'a [String]) -> RepoSpec<'a> {
        RepoSpec {
            name,
            kind: if members.is_empty() {
                RepoKind::Hosted
            } else {
                RepoKind::Group
            },
            format: Format::Npm,
            visibility: Visibility::Private,
            upstream: None,
            members,
        }
    }

    fn by() -> Actor<'static> {
        Actor {
            user_id: Some(1),
            username: "root",
            admin: true,
        }
    }

    async fn create(db: &FakeDb, spec: &RepoSpec<'_>) -> AppResult<Repository> {
        CreateRepository::new(db.repositories(), db.audit(), crate::server::event_bus())
            .run(spec, &by(), Utc::now())
            .await
    }

    /// The name is taken before anything else is looked at, and the refusal
    /// leaves neither a second row nor a second audit entry.
    #[tokio::test]
    async fn creating_a_repository_twice_is_a_conflict() {
        let db = FakeDb::new();
        create(&db, &spec("npm-hosted", &[])).await.unwrap();

        let again = create(&db, &spec("npm-hosted", &[])).await.unwrap_err();

        assert!(matches!(again, AppError::Conflict(_)), "{again:?}");
        assert_eq!(db.repositories().all().await.unwrap().len(), 1);
        assert_eq!(db.audit_rows().len(), 1);
    }

    /// A definition that could not be served is refused before the row is
    /// written, so nothing is recorded either.
    #[tokio::test]
    async fn an_unservable_definition_is_refused_before_the_write() {
        let db = FakeDb::new();
        let members = vec!["missing".to_string()];

        let refused = create(&db, &spec("npm-all", &members)).await.unwrap_err();

        assert!(matches!(refused, AppError::BadRequest(_)), "{refused:?}");
        assert!(db.repositories().all().await.unwrap().is_empty());
        assert!(db.audit_rows().is_empty());
    }

    /// What a delete consults before it asks the store: the groups still
    /// listing the name, which is what turns the removal into a 409.
    #[tokio::test]
    async fn a_member_is_reported_by_every_group_holding_it() {
        let db = FakeDb::new();
        create(&db, &spec("npm-hosted", &[])).await.unwrap();
        let members = vec!["npm-hosted".to_string()];
        create(&db, &spec("npm-all", &members)).await.unwrap();

        let repos = db.repositories();
        assert_eq!(
            groups_containing(repos.as_ref(), "npm-hosted").await.unwrap(),
            vec!["npm-all".to_string()]
        );
        assert!(groups_containing(repos.as_ref(), "npm-all")
            .await
            .unwrap()
            .is_empty());
    }
}
