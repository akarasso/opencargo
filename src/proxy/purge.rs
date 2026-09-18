use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use tracing::warn;

use crate::domain::{RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{CacheRepo, MAX_GROUP_DEPTH};
use crate::server::AppState;

/// Proxy: rows, legacy meta and files; Group: every proxy member, nested
/// groups included; Hosted: 400.
pub async fn purge_repository(state: &AppState, repo: &Repository) -> AppResult<()> {
    match repo.kind()? {
        RepoKind::Hosted => Err(AppError::BadRequest(
            "can only purge cache on proxy or group repositories".to_string(),
        )),
        RepoKind::Proxy => state.proxy.purge_repo(CacheRepo(repo)).await,
        RepoKind::Group => purge_members(state, repo, 0, &mut HashSet::from([repo.id])).await,
    }
}

fn purge_members<'a>(
    state: &'a AppState,
    group: &'a Repository,
    depth: u32,
    seen: &'a mut HashSet<i64>,
) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'a>> {
    Box::pin(async move {
        if depth >= MAX_GROUP_DEPTH {
            return Err(AppError::Internal(
                "group nesting depth exceeded".to_string(),
            ));
        }
        for name in group.members() {
            let Some(member) = state.repos.by_name(&name).await? else {
                warn!(group = %group.name, member = %name, "group member not found; skipping purge");
                continue;
            };
            if !seen.insert(member.id) {
                continue;
            }
            match member.kind()? {
                RepoKind::Proxy => state.proxy.purge_repo(CacheRepo(&member)).await?,
                RepoKind::Group => purge_members(state, &member, depth + 1, seen).await?,
                RepoKind::Hosted => {}
            }
        }
        Ok(())
    })
}
