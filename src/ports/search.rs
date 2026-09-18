//! Finding a package by what it is called or what it says about itself.
//!
//! One read method, because each adapter maintains its own index inside its
//! own writes -- SQLite through the `packages_fts` triggers, another dialect
//! in the same statement -- and what the layers above depend on is only that
//! a search after a publish finds the package.

use async_trait::async_trait;

use crate::domain::Package;
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
