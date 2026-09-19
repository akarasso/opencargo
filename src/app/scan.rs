//! Scanning a version for known vulnerabilities.
//!
//! The order is the whole of the use case: ask the feed, *then* write. An
//! OSV round trip is seconds long and SQLite has one writer, so a scan that
//! held a transaction open across it would stall every concurrent publish.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::info;

use crate::domain::{Format, ScanResult};
use crate::error::StoreError;
use crate::ports::vulns::{ScanError, VulnFeed, VulnStore};

pub struct ScanVersion {
    feed: Arc<dyn VulnFeed>,
    store: Arc<dyn VulnStore>,
}

impl ScanVersion {
    pub fn new(feed: Arc<dyn VulnFeed>, store: Arc<dyn VulnStore>) -> Self {
        Self { feed, store }
    }

    /// Assess then record. A disabled feed reports clean and records
    /// nothing: a row saying "clean" would claim a version was looked at.
    pub async fn run(
        &self,
        version: i64,
        metadata_json: &str,
        format: Format,
        now: DateTime<Utc>,
    ) -> Result<ScanResult, ScanError> {
        let result = self.feed.assess(metadata_json, format).await?;
        if self.feed.enabled() {
            self.record(version, &result, now).await?;
        }
        Ok(result)
    }

    /// Record a result the caller already has, which is what a publish that
    /// scanned before serving the artifact holds.
    pub async fn record(
        &self,
        version: i64,
        result: &ScanResult,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.store.record(version, result, now).await?;
        info!(
            version_id = version,
            total_deps = result.total_deps,
            vulnerable_deps = result.vulnerable_deps,
            status = %result.status,
            "Vulnerability scan completed"
        );
        Ok(())
    }

    /// Drop what a version was previously found to have, so a rescan does
    /// not read as a second finding.
    pub async fn forget(&self, version: i64) -> Result<(), StoreError> {
        self.store.forget(version).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Severity, VulnDetail};
    use crate::testing::fakes::FakeDb;
    use async_trait::async_trait;

    /// A feed that answers from a fixed result, and counts the calls.
    struct Feed {
        enabled: bool,
        result: ScanResult,
    }

    #[async_trait]
    impl VulnFeed for Feed {
        fn enabled(&self) -> bool {
            self.enabled
        }

        async fn assess_batch(
            &self,
            _format: Format,
            deps: &[(String, String)],
        ) -> Result<Vec<Vec<VulnDetail>>, ScanError> {
            Ok(vec![Vec::new(); deps.len()])
        }

        async fn assess(
            &self,
            _metadata_json: &str,
            _format: Format,
        ) -> Result<ScanResult, ScanError> {
            Ok(self.result.clone())
        }
    }

    fn finding() -> ScanResult {
        ScanResult {
            total_deps: 3,
            vulnerable_deps: 1,
            status: "warning".to_string(),
            details: vec![VulnDetail {
                dependency: "left-pad".to_string(),
                version: "1.0.0".to_string(),
                vuln_id: "GHSA-x".to_string(),
                summary: "bad".to_string(),
                severity: Severity::High,
                score: Some(7.5),
            }],
        }
    }

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-18T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn a_scan_records_what_the_feed_found_under_the_callers_clock() {
        let db = FakeDb::new();
        let feed = Arc::new(Feed {
            enabled: true,
            result: finding(),
        });
        let scan = ScanVersion::new(feed, db.vulns());

        let result = scan.run(7, "{}", Format::Npm, at()).await.unwrap();

        assert_eq!(result.vulnerable_deps, 1);
        let stored = db.vulns().latest(7).await.unwrap().unwrap();
        assert_eq!(stored.scanned_at, at());
        assert_eq!((stored.total_deps, stored.vulnerable_deps), (3, 1));
        assert_eq!(stored.status, "warning");
    }

    /// A disabled feed has not looked: a stored "clean" would say it had.
    #[tokio::test]
    async fn a_disabled_feed_records_nothing() {
        let db = FakeDb::new();
        let feed = Arc::new(Feed {
            enabled: false,
            result: ScanResult::clean(),
        });
        let scan = ScanVersion::new(feed, db.vulns());

        let result = scan.run(7, "{}", Format::Npm, at()).await.unwrap();

        assert_eq!(result.status, "clean");
        assert!(db.vulns().latest(7).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forgetting_a_version_leaves_a_rescan_with_one_finding() {
        let db = FakeDb::new();
        let feed = Arc::new(Feed {
            enabled: true,
            result: finding(),
        });
        let scan = ScanVersion::new(feed, db.vulns());
        scan.run(7, "{}", Format::Npm, at()).await.unwrap();

        scan.forget(7).await.unwrap();
        assert!(db.vulns().latest(7).await.unwrap().is_none());

        scan.run(7, "{}", Format::Npm, at()).await.unwrap();
        assert!(db.vulns().latest(7).await.unwrap().is_some());
    }
}
