//! Port 17, `NugetFeedRead` (A1 C1): NuGet's filtered search, and nothing
//! else — every other NuGet read goes through `PackageStore`.
//!
//! The filters are version facts the NuGet adapter writes into a version's
//! `metadata_json` at publish: `prerelease`, `semver2` and `packageTypes`
//! (lowercase names). They are evaluated before the window is cut, only
//! listed (not yanked) versions count, and the total is the number of
//! packages that survived the filters.

use async_trait::async_trait;
use serde::Deserialize;

use crate::domain::{Package, Version};
use crate::error::StoreError;

#[derive(Debug, Clone, Default)]
pub struct FeedQuery<'a> {
    pub repository: i64,
    pub text: Option<&'a str>,
    pub skip: u32,
    pub take: u32,
    pub prerelease: bool,
    pub semver2: bool,
    pub package_type: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct FeedHit {
    pub package: Package,
    /// Listed versions that pass the filters, in publication order.
    pub versions: Vec<Version>,
    pub downloads: i64,
}

#[derive(Debug, Clone, Default)]
pub struct FeedPage {
    pub total: u64,
    pub hits: Vec<FeedHit>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Facts {
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    semver2: bool,
    #[serde(default)]
    package_types: Vec<String>,
}

impl FeedQuery<'_> {
    fn tokens(&self) -> Vec<String> {
        self.text
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_lowercase)
            .collect()
    }

    pub fn matches(&self, package: &Package) -> bool {
        self.tokens().iter().all(|t| {
            package.name.to_lowercase().contains(t)
                || package
                    .description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(t))
        })
    }

    pub fn admits(&self, version: &Version) -> bool {
        if version.yanked {
            return false;
        }
        let facts: Facts = serde_json::from_str(&version.metadata_json).unwrap_or_default();
        (self.prerelease || !facts.prerelease)
            && (self.semver2 || !facts.semver2)
            && self.package_type.is_none_or(|wanted| {
                facts
                    .package_types
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(wanted))
            })
    }

    /// The one pagination both adapters apply to their candidates: filter,
    /// order by name, count, then cut the window.
    pub fn page(&self, candidates: Vec<(Package, Vec<Version>, i64)>) -> FeedPage {
        let mut kept: Vec<FeedHit> = candidates
            .into_iter()
            .filter(|(package, _, _)| self.matches(package))
            .filter_map(|(package, versions, downloads)| {
                let versions: Vec<Version> =
                    versions.into_iter().filter(|v| self.admits(v)).collect();
                (!versions.is_empty()).then_some(FeedHit {
                    package,
                    versions,
                    downloads,
                })
            })
            .collect();
        kept.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        let total = kept.len() as u64;
        let hits = kept
            .into_iter()
            .skip(self.skip as usize)
            .take(self.take as usize)
            .collect();
        FeedPage { total, hits }
    }
}

#[async_trait]
pub trait NugetFeedRead: Send + Sync {
    async fn search(&self, query: &FeedQuery<'_>) -> Result<FeedPage, StoreError>;
}
