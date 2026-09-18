use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use super::store::{self, IdRange, ReportFilter, Totals};

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
        pool: &SqlitePool,
        f: &ReportFilter<'_>,
        key: String,
    ) -> Result<Totals, sqlx::Error> {
        let upto = store::max_id(pool).await?;
        let base = self.base(&key);
        let Some((after, mut totals)) = base else {
            return self.scan(pool, f, key, upto).await;
        };
        if upto > after {
            let delta = store::report_totals(pool, f, IdRange { after, upto }).await?;
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
        pool: &SqlitePool,
        f: &ReportFilter<'_>,
        key: String,
        upto: i64,
    ) -> Result<Totals, sqlx::Error> {
        #[cfg(test)]
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let at = Instant::now();
        let totals = store::report_totals(pool, f, IdRange { after: 0, upto }).await?;
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
    use crate::testing::fixture::Fx;

    async fn row(fx: &Fx, verdict: &str) {
        let flag = verdict == "would_block";
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO policy_resolutions (requested_repo, member_repo, format, name, actor, actor_kind, would_block)
             VALUES ('p', 'p', 'npm', 'widget', 'anonymous', 'anonymous', ?1) RETURNING id",
        )
        .bind(flag)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO policy_verdicts (resolution_id, rule, verdict) VALUES (?1, 'typosquat', ?2)")
            .bind(id)
            .bind(verdict)
            .execute(&fx.pool)
            .await
            .unwrap();
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
        let fx = Fx::new().await;
        let cache = TotalsCache::default();
        row(&fx, "pass").await;
        row(&fx, "would_block").await;
        let first = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!((first.resolutions, first.would_block), (2, 1));
        assert_eq!(first.by_rule["typosquat"].pass, 1);
        let again = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!(again, first, "no row landed: the snapshot answers");
        row(&fx, "would_block").await;
        row(&fx, "unknown").await;
        let grown = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!((grown.resolutions, grown.would_block), (4, 2));
        assert_eq!(grown.by_rule["typosquat"].would_block, 2);
        assert_eq!(grown.by_rule["typosquat"].unknown, 1);
        assert_eq!(
            cache.scans.load(Ordering::SeqCst),
            1,
            "three refetches, one scan of the window"
        );
        let other = cache
            .totals(&fx.pool, &filter(), "other".into())
            .await
            .unwrap();
        assert_eq!(other, grown, "another filter key scans on its own");
        assert_eq!(cache.scans.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn erasure_forgets_the_snapshot() {
        let fx = Fx::new().await;
        let cache = TotalsCache::default();
        row(&fx, "pass").await;
        row(&fx, "pass").await;
        let two = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!(two.resolutions, 2);
        sqlx::query("DELETE FROM policy_resolutions")
            .execute(&fx.pool)
            .await
            .unwrap();
        let stale = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!(
            stale.resolutions, 2,
            "a snapshot outlives a delete until told"
        );
        cache.forget();
        let none = cache.totals(&fx.pool, &filter(), "k".into()).await.unwrap();
        assert_eq!(none.resolutions, 0);
    }
}
