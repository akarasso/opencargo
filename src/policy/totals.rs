use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::StoreError;
use crate::ports::policy::{IdRange, PolicyStore, ReportFilter, Totals};

/// A full scan of the window is repeated no sooner than this, and no
/// sooner than `AMORTISE` times what it cost the last time.
const FLOOR: Duration = Duration::from_secs(5);
const AMORTISE: u32 = 10;

struct Snapshot {
    at: Instant,
    ttl: Duration,
    upto: i64,
    totals: Totals,
}

/// Report totals per filter: one scan of the window, then only the rows
/// inserted since, so a page refetching on every flush costs one short
/// range read per refetch, and nothing when no row landed.
#[derive(Default)]
pub struct TotalsCache {
    snapshots: Mutex<HashMap<String, Snapshot>>,
    #[cfg(test)]
    pub(crate) scans: std::sync::atomic::AtomicUsize,
}

impl TotalsCache {
    pub async fn totals(
        &self,
        store: &dyn PolicyStore,
        f: &ReportFilter<'_>,
        key: String,
    ) -> Result<Totals, StoreError> {
        let upto = store.max_id().await?;
        let base = self.base(&key);
        let Some((after, mut totals)) = base else {
            return self.scan(store, f, key, upto).await;
        };
        if upto > after {
            let delta = store.totals(f, IdRange { after, upto }).await?;
            totals.absorb(delta);
            let mut snapshots = self.snapshots.lock().unwrap();
            if let Some(s) = snapshots.get_mut(&key).filter(|s| s.upto == after) {
                s.upto = upto;
                s.totals = totals.clone();
            }
        }
        Ok(totals)
    }

    /// Erasure removed rows a snapshot counted.
    pub fn forget(&self) {
        self.snapshots.lock().unwrap().clear();
    }

    fn base(&self, key: &str) -> Option<(i64, Totals)> {
        let snapshots = self.snapshots.lock().unwrap();
        let s = snapshots.get(key).filter(|s| s.at.elapsed() < s.ttl)?;
        Some((s.upto, s.totals.clone()))
    }

    async fn scan(
        &self,
        store: &dyn PolicyStore,
        f: &ReportFilter<'_>,
        key: String,
        upto: i64,
    ) -> Result<Totals, StoreError> {
        #[cfg(test)]
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let at = Instant::now();
        let totals = store.totals(f, IdRange { after: 0, upto }).await?;
        let ttl = (at.elapsed() * AMORTISE).max(FLOOR);
        self.snapshots.lock().unwrap().insert(
            key,
            Snapshot {
                at,
                ttl,
                upto,
                totals: totals.clone(),
            },
        );
        Ok(totals)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use chrono::Utc;

    use super::*;
    use crate::domain::{RuleVerdict, Verdict};
    use crate::ports::policy::NewResolution;
    use crate::testing::fakes::FakeDb;

    async fn row(store: &dyn PolicyStore, verdict: Verdict) {
        let verdicts = [RuleVerdict::new("typosquat", verdict, "")];
        let row = NewResolution {
            requested_repo: "p",
            member_repo: "p",
            format: "npm",
            name: "widget",
            version: None,
            digest: None,
            published_at: None,
            date_source: "",
            actor: "anonymous",
            actor_kind: "anonymous",
            user_id: None,
            verdicts: &verdicts,
        };
        store.insert_batch(&[row], Utc::now()).await.unwrap();
    }

    fn filter() -> ReportFilter<'static> {
        ReportFilter {
            since: Utc::now() - chrono::Duration::hours(24),
            repo: None,
            rule: None,
            subject: None,
        }
    }

    #[tokio::test]
    async fn refetches_between_flushes_never_rescan_the_window() {
        let db = FakeDb::new();
        let store = db.policy();
        let cache = TotalsCache::default();
        row(store.as_ref(), Verdict::Pass).await;
        row(store.as_ref(), Verdict::WouldBlock).await;
        let first = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!((first.resolutions, first.would_block), (2, 1));
        assert_eq!(first.by_rule["typosquat"].pass, 1);
        let again = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!(again, first, "no row landed: the snapshot answers");
        row(store.as_ref(), Verdict::WouldBlock).await;
        row(store.as_ref(), Verdict::Unknown).await;
        let grown = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!((grown.resolutions, grown.would_block), (4, 2));
        assert_eq!(grown.by_rule["typosquat"].would_block, 2);
        assert_eq!(grown.by_rule["typosquat"].unknown, 1);
        assert_eq!(
            cache.scans.load(Ordering::SeqCst),
            1,
            "three refetches, one scan of the window"
        );
        let other = cache
            .totals(store.as_ref(), &filter(), "other".into())
            .await
            .unwrap();
        assert_eq!(other, grown, "another filter key scans on its own");
        assert_eq!(cache.scans.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn erasure_forgets_the_snapshot() {
        let db = FakeDb::new();
        let store = db.policy();
        let cache = TotalsCache::default();
        row(store.as_ref(), Verdict::Pass).await;
        row(store.as_ref(), Verdict::Pass).await;
        let two = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!(two.resolutions, 2);
        store.delete_older_than(0, Utc::now()).await.unwrap();
        let stale = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!(
            stale.resolutions, 2,
            "a snapshot outlives a delete until told"
        );
        cache.forget();
        let none = cache
            .totals(store.as_ref(), &filter(), "k".into())
            .await
            .unwrap();
        assert_eq!(none.resolutions, 0);
    }
}
