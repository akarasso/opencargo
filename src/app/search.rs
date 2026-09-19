//! Finding a package the server can serve: the hosted rows, and what its
//! proxy members have been seen serving.
//!
//! Merging the two indexes is a rule, not a rendering: a hosted package and a
//! proxied one of the same name are one answer, and which of them the caller
//! is told about decides where they will be told to fetch it from. So it lives
//! here rather than in the handler that encodes it.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::domain::Sighting;
use crate::error::StoreError;
use crate::ports::dashboard::DashboardRead;
use crate::ports::repositories::RepositoryStore;
use crate::ports::search::{CachedPackageIndex, SearchIndex, SearchQuery, SearchScope};

/// Where the answer comes from, which is where the client will have to fetch
/// it: a hosted row this server owns, or a package a proxy member served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Hosted,
    Cached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    pub description: Option<String>,
    /// The latest release for a hosted package, the newest version last seen
    /// for a proxied one.
    pub version: Option<String>,
    pub repository: Option<String>,
    pub source: Source,
    /// When the proxy last served it; `None` for a hosted row.
    pub last_seen: Option<DateTime<Utc>>,
}

pub struct Find {
    hosted: Arc<dyn SearchIndex>,
    cached: Arc<dyn CachedPackageIndex>,
    dashboard: Arc<dyn DashboardRead>,
    repos: Arc<dyn RepositoryStore>,
}

impl Find {
    pub fn new(
        hosted: Arc<dyn SearchIndex>,
        cached: Arc<dyn CachedPackageIndex>,
        dashboard: Arc<dyn DashboardRead>,
        repos: Arc<dyn RepositoryStore>,
    ) -> Self {
        Self {
            hosted,
            cached,
            dashboard,
            repos,
        }
    }

    pub async fn run(
        &self,
        query: &SearchQuery,
        scope: SearchScope,
        limit: u32,
    ) -> Result<Vec<Hit>, StoreError> {
        let found = self.hosted.search(scope, Some(query), limit).await?;
        let cached = self.cached.search(scope, Some(query), limit).await?;
        let latest = self
            .dashboard
            .latest_versions(&found.iter().map(|pkg| pkg.id).collect::<Vec<_>>())
            .await?;
        let names = self.repository_names().await?;

        let hosted = found.into_iter().map(|pkg| Hit {
            name: pkg.name,
            description: pkg.description,
            version: latest.get(&pkg.id).cloned(),
            repository: names.get(&pkg.repository_id).cloned(),
            source: Source::Hosted,
            last_seen: None,
        });
        let proxied = cached.into_iter().map(|row| Hit {
            name: row.name,
            description: row.description,
            version: row.latest_version,
            repository: names.get(&row.repository_id).cloned(),
            source: Source::Cached,
            last_seen: Some(row.last_seen_at),
        });
        Ok(merge(hosted, proxied, limit))
    }

    async fn repository_names(&self) -> Result<HashMap<i64, String>, StoreError> {
        Ok(self
            .repos
            .all()
            .await?
            .into_iter()
            .map(|repo| (repo.id, repo.name))
            .collect())
    }
}

/// Hosted answers first, each name once: a package this server hosts is where
/// the client should get it, and the same name cached behind two proxy members
/// is still one package.
fn merge(
    hosted: impl Iterator<Item = Hit>,
    cached: impl Iterator<Item = Hit>,
    limit: u32,
) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    hosted
        .chain(cached)
        .filter(|hit| seen.insert(hit.name.clone()))
        .take(limit as usize)
        .collect()
}

/// Remember a package a proxy member answered for.
///
/// `exchanged` is what keeps this off the hot path: a warm cache hit serves
/// without talking to the upstream and writes nothing, so the index costs at
/// most one write per package per TTL. A failure is logged and dropped — the
/// client has its package, and an index is not part of the release chain.
pub async fn remember(
    index: &dyn CachedPackageIndex,
    exchanged: bool,
    seen: &Sighting<'_>,
    now: DateTime<Utc>,
) {
    if !exchanged {
        return;
    }
    if let Err(err) = index.remember(seen, now).await {
        warn!(
            package = seen.name,
            error = %err,
            "a proxied package was served but not indexed for search"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(name: &str, source: Source) -> Hit {
        Hit {
            name: name.to_string(),
            description: None,
            version: None,
            repository: None,
            source,
            last_seen: None,
        }
    }

    #[test]
    fn a_hosted_package_wins_the_name_a_proxy_also_serves() {
        let merged = merge(
            vec![hit("left-pad", Source::Hosted)].into_iter(),
            vec![hit("left-pad", Source::Cached), hit("lodash", Source::Cached)].into_iter(),
            10,
        );
        let names: Vec<(&str, Source)> = merged
            .iter()
            .map(|hit| (hit.name.as_str(), hit.source))
            .collect();
        assert_eq!(
            names,
            [("left-pad", Source::Hosted), ("lodash", Source::Cached)]
        );
    }

    #[test]
    fn one_name_cached_behind_two_members_is_one_answer_and_the_limit_holds() {
        let merged = merge(
            std::iter::empty(),
            vec![
                hit("lodash", Source::Cached),
                hit("lodash", Source::Cached),
                hit("axios", Source::Cached),
            ]
            .into_iter(),
            1,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "lodash");
    }
}
