//! Finding a package by what it is called or what it says about itself.
//!
//! One read method, because each adapter maintains its own index inside its
//! own writes -- SQLite through the `packages_fts` triggers, another dialect
//! in the same statement -- and what the layers above depend on is only that
//! a search after a publish finds the package.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{CachedPackage, Package, Sighting};
use crate::error::StoreError;

/// Which packages a search may return. `PublicOnly` and `All` are the two
/// arms the cross-repository search has, and they are arms rather than a
/// visibility value because "at least private" is not one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Repo(i64),
    PublicOnly,
    All,
}

/// A query that has something to match: a list of tokens, non-empty *after*
/// sanitisation.
///
/// [`SearchQuery::parse`] returning `None` is what keeps a whitespace-only or
/// quote-only query from reaching an index that would refuse it -- the caller
/// answers with an empty result instead, which is what the deleted `LIKE`
/// fallback amounted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery(Vec<String>);

impl SearchQuery {
    pub fn parse(text: &str) -> Option<Self> {
        let tokens: Vec<String> = text
            .split_whitespace()
            .map(|word| word.replace('"', ""))
            .filter(|word| !word.is_empty())
            .collect();
        (!tokens.is_empty()).then_some(Self(tokens))
    }

    pub fn tokens(&self) -> &[String] {
        &self.0
    }
}

#[async_trait]
pub trait SearchIndex: Send + Sync {
    /// `Some(q)`: relevance-ordered, most relevant first. `None`: a browse --
    /// the scope's packages in the adapter's natural order, which is npm's
    /// no-`text` default and is never empty for a populated scope.
    async fn search(
        &self,
        scope: SearchScope,
        query: Option<&SearchQuery>,
        limit: u32,
    ) -> Result<Vec<Package>, StoreError>;
}

/// Port 24 (A1 C1, proposed at its next revision): what the proxy has been
/// seen serving, so a search answers for the packages this server served and
/// not only for the ones it hosts.
///
/// A separate port rather than a write side on [`SearchIndex`]: `packages_fts`
/// is maintained by the adapter's own `packages` writes, while nothing writes
/// these rows unless the proxy path says so, and what they answer is a
/// different read model with a lifecycle of its own.
#[async_trait]
pub trait CachedPackageIndex: Send + Sync {
    /// Idempotent under (repository, name): a second sighting of a package
    /// refreshes what the document said and moves `last_seen_at`.
    async fn remember(&self, seen: &Sighting<'_>, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// `Some(q)`: relevance-ordered. `None`: a browse, the scope's rows in the
    /// adapter's natural order -- [`SearchIndex::search`]'s contract, over the
    /// other index.
    async fn search(
        &self,
        scope: SearchScope,
        query: Option<&SearchQuery>,
        limit: u32,
    ) -> Result<Vec<CachedPackage>, StoreError>;

    /// What a cache purge drops, since a purge is the operator saying the
    /// repository has served nothing; eviction does not, because the next
    /// request fetches the package again.
    async fn forget_repo(&self, repo: i64) -> Result<u64, StoreError>;
}
