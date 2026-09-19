use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use tracing::warn;

use crate::domain::{CacheRepo, MAX_GROUP_DEPTH};
use crate::domain::{RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::repositories::RepositoryStore;
use crate::proxy::engine::ProxyEngine;

/// Proxy: rows, legacy meta and files; Group: every proxy member, nested
/// groups included; Hosted: 400. Answers with the proxy repositories emptied,
/// which is what a caller with another read model of the cache has to forget.
pub async fn purge_repository(
    proxy: &ProxyEngine,
    repos: &dyn RepositoryStore,
    repo: &Repository,
) -> AppResult<Vec<i64>> {
    let mut purged = Vec::new();
    match repo.kind()? {
        RepoKind::Hosted => {
            return Err(AppError::BadRequest(
                "can only purge cache on proxy or group repositories".to_string(),
            ))
        }
        RepoKind::Proxy => {
            proxy.purge_repo(CacheRepo(repo)).await?;
            purged.push(repo.id);
        }
        RepoKind::Group => {
            purge_members(
                proxy,
                repos,
                repo,
                0,
                &mut HashSet::from([repo.id]),
                &mut purged,
            )
            .await?
        }
    }
    Ok(purged)
}

fn purge_members<'a>(
    proxy: &'a ProxyEngine,
    repos: &'a dyn RepositoryStore,
    group: &'a Repository,
    depth: u32,
    seen: &'a mut HashSet<i64>,
    purged: &'a mut Vec<i64>,
) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'a>> {
    Box::pin(async move {
        if depth >= MAX_GROUP_DEPTH {
            return Err(AppError::Internal(
                "group nesting depth exceeded".to_string(),
            ));
        }
        for name in group.members() {
            let Some(member) = repos.by_name(&name).await? else {
                warn!(group = %group.name, member = %name, "group member not found; skipping purge");
                continue;
            };
            if !seen.insert(member.id) {
                continue;
            }
            match member.kind()? {
                RepoKind::Proxy => {
                    proxy.purge_repo(CacheRepo(&member)).await?;
                    purged.push(member.id);
                }
                RepoKind::Group => {
                    purge_members(proxy, repos, &member, depth + 1, seen, purged).await?
                }
                RepoKind::Hosted => {}
            }
        }
        Ok(())
    })
}
