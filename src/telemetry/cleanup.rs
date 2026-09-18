use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{error, info, warn};

use crate::config::CleanupConfig;
use crate::ports::packages::PackageStore;
use crate::ports::policy::PolicyStore;
use crate::ports::proxy_cache::ProxyCacheStore;
use crate::storage::StorageBackend;

/// Abandoned `*.part-*` files older than this are reclaimed by the sweep.
const STALE_PART_AGE: Duration = Duration::from_secs(3600);

/// A retention bound in days, as the duration the ports take.
fn retention(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(86_400))
}

/// How many cache rows one sweep may evict. A daily pass over a cache with
/// more expired rows than this comes back the next day for the rest, which is
/// the point: the sweep never holds an unbounded result set.
const SWEEP_LIMIT: u32 = 10_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepStats {
    pub rows: u64,
    pub files: u64,
    pub parts: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CleanupStats {
    pub prereleases: Option<u64>,
    pub proxy: Option<SweepStats>,
    pub policy: Option<u64>,
}

fn proxy_idle_days(config: &CleanupConfig) -> Option<u64> {
    config.proxy_cache_older_than_days.filter(|days| *days > 0)
}

fn policy_days(config: &CleanupConfig) -> Option<u64> {
    config
        .policy_report_older_than_days
        .filter(|days| *days > 0)
}

/// The task starts when any sweep has something to do: pre-releases under
/// `enabled`, the proxy cache or the policy report under their own bound.
pub(crate) fn sweeps_configured(config: &CleanupConfig) -> bool {
    config.enabled || proxy_idle_days(config).is_some() || policy_days(config).is_some()
}

/// Start a background cleanup task that runs every 24 hours: the pre-release
/// sweep when `enabled`, the proxy cache and policy report sweeps whenever
/// their bound is set.
pub async fn start_cleanup_task(
    packages: Arc<dyn PackageStore>,
    cache: Arc<dyn ProxyCacheStore>,
    policy: Arc<dyn PolicyStore>,
    storage: Arc<dyn StorageBackend>,
    config: CleanupConfig,
) {
    if !sweeps_configured(&config) {
        info!("Cleanup task is disabled");
        return;
    }

    info!("Cleanup task started (runs every 24h)");
    loop {
        // Run first, THEN sleep: a service restarted more often than daily
        // (common under k8s) would otherwise never clean up at all.
        let sweeps = RunCleanup {
            packages: packages.as_ref(),
            cache: cache.as_ref(),
            policy: policy.as_ref(),
            storage: &storage,
        };
        sweeps.run(&config, Utc::now()).await;
        tokio::time::sleep(Duration::from_secs(86400)).await;
    }
}

/// The daily sweeps, over the ports they reach the world through.
pub(crate) struct RunCleanup<'a> {
    pub packages: &'a dyn PackageStore,
    pub cache: &'a dyn ProxyCacheStore,
    pub policy: &'a dyn PolicyStore,
    pub storage: &'a Arc<dyn StorageBackend>,
}

impl RunCleanup<'_> {
    pub(crate) async fn run(&self, config: &CleanupConfig, now: DateTime<Utc>) -> CleanupStats {
        info!("Running scheduled cleanup");
        let mut stats = CleanupStats::default();

        if let Some(days) = config.prerelease_older_than_days.filter(|_| config.enabled) {
            match sweep_prereleases(self.packages, self.storage, days, now).await {
                Ok(deleted) => stats.prereleases = Some(deleted),
                Err(e) => error!(error = %e, "Failed to clean up old pre-release versions"),
            }
        }

        if let Some(idle) = proxy_idle_days(config) {
            match sweep_proxy_cache(self.cache, self.storage, idle, now).await {
                Ok(sweep) => stats.proxy = Some(sweep),
                Err(e) => error!(error = %e, "Failed to sweep the proxy cache"),
            }
        }

        if let Some(days) = policy_days(config) {
            match self.policy.delete_older_than(days, now).await {
                Ok(deleted) => {
                    info!(deleted, days, "Policy report sweep complete");
                    stats.policy = Some(deleted);
                }
                Err(e) => error!(error = %e, "Failed to sweep the policy report"),
            }
        }
        stats
    }
}

/// Drop the pre-release versions past their retention, artifact first: the
/// version row carries the only record of its path, so losing the row first
/// would strand the file with nothing left pointing at it.
pub(crate) async fn sweep_prereleases(
    packages: &dyn PackageStore,
    storage: &Arc<dyn StorageBackend>,
    older_than_days: u64,
    now: DateTime<Utc>,
) -> anyhow::Result<u64> {
    let stale = packages
        .stale_prereleases(retention(older_than_days), now)
        .await?;
    if stale.is_empty() {
        info!("No old pre-release versions to clean up");
        return Ok(0);
    }

    info!(count = stale.len(), "Cleaning up old pre-release versions");
    for row in &stale {
        if let Err(e) = storage.delete(&row.tarball_path).await {
            warn!(
                version_id = row.id,
                path = %row.tarball_path,
                error = %e,
                "Failed to delete tarball for pre-release version"
            );
        }
        packages.delete_version(row.id).await?;
        info!(
            package = %row.package,
            version = %row.version,
            "Deleted old pre-release version"
        );
    }
    Ok(stale.len() as u64)
}

/// Evict expired negative entries and every row idle for `idle_days`, file
/// first, then row; then reclaim abandoned part files.
pub(crate) async fn sweep_proxy_cache(
    cache: &dyn ProxyCacheStore,
    storage: &Arc<dyn StorageBackend>,
    idle_days: u64,
    now: DateTime<Utc>,
) -> anyhow::Result<SweepStats> {
    let mut stats = SweepStats::default();
    for row in cache.evictable(retention(idle_days), now, SWEEP_LIMIT).await? {
        if let Some(path) = &row.storage_path {
            match storage.delete(path).await {
                Ok(()) => stats.files += 1,
                Err(e) => warn!(path, error = %e, "Failed to delete an evicted cache file"),
            }
        }
        cache.delete(row.id).await?;
        stats.rows += 1;
    }
    stats.parts = storage
        .remove_stale_parts("", STALE_PART_AGE)
        .await?;
    info!(
        rows = stats.rows,
        files = stats.files,
        parts = stats.parts,
        "Proxy cache sweep complete"
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use crate::domain::{Format, NewEntry, RepoKind, RepoSpec, RuleVerdict, Verdict, Visibility};
    use crate::ports::packages::{NameMatch, NewRelease};
    use crate::ports::policy::{NewResolution, ReportFilter};
    use crate::testing::fakes::FakeDb;

    /// The instant the sweeps run at; every row is written relative to it, so
    /// no test sleeps and none asks a store what time it is.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap()
    }

    fn days_ago(n: i64) -> DateTime<Utc> {
        now() - chrono::TimeDelta::days(n)
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        packages: Arc<dyn PackageStore>,
        cache: Arc<dyn ProxyCacheStore>,
        policy: Arc<dyn PolicyStore>,
        storage: Arc<dyn StorageBackend>,
        /// The seeded repositories: a hosted npm one, a hosted go one, and
        /// the npm proxy whose answers the cache rows belong to.
        npm: i64,
        go: i64,
        proxy: i64,
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = FakeDb::new();
        let repos = db.repositories();
        let mut ids = Vec::new();
        for (name, kind, format, upstream) in [
            ("npmrepo", RepoKind::Hosted, Format::Npm, None),
            ("gorepo", RepoKind::Hosted, Format::Go, None),
            (
                "p",
                RepoKind::Proxy,
                Format::Npm,
                Some("https://registry.npmjs.org"),
            ),
        ] {
            let spec = RepoSpec {
                name,
                kind,
                format,
                visibility: Visibility::Public,
                upstream,
                members: &[],
            };
            ids.push(repos.create(&spec, now()).await.unwrap().id);
        }
        Fx {
            storage: crate::storage::filesystem(tmp.path().join("storage")),
            packages: db.packages(),
            cache: db.proxy_cache(),
            policy: db.policy(),
            _tmp: tmp,
            npm: ids[0],
            go: ids[1],
            proxy: ids[2],
        }
    }

    impl Fx {
        fn sweeps(&self) -> RunCleanup<'_> {
            RunCleanup {
                packages: self.packages.as_ref(),
                cache: self.cache.as_ref(),
                policy: self.policy.as_ref(),
                storage: &self.storage,
            }
        }

        /// A resolution and its verdict, dated by the caller's clock like
        /// every other row here — `delete_older_than` compares `created_at`
        /// against the cut-off the sweep is handed, not one a store reads.
        async fn policy_row(&self, name: &str, at: DateTime<Utc>) {
            self.policy
                .insert_batch(
                    &[NewResolution {
                        requested_repo: "p",
                        member_repo: "p",
                        format: "npm",
                        name,
                        version: None,
                        digest: None,
                        published_at: None,
                        date_source: "none",
                        actor: "anonymous",
                        actor_kind: "anonymous",
                        user_id: None,
                        verdicts: &[RuleVerdict::new("typosquat", Verdict::Pass, "")],
                    }],
                    at,
                )
                .await
                .unwrap();
        }

        /// What the report still lists, newest first, and how many verdicts
        /// hang off those rows.
        async fn policy_left(&self) -> (Vec<String>, usize) {
            let filter = ReportFilter {
                since: days_ago(3650),
                repo: None,
                rule: None,
                subject: None,
            };
            let rows = self.policy.resolutions(&filter, 1, 50).await.unwrap();
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let verdicts = self.policy.verdicts_for(&ids, None).await.unwrap();
            (rows.into_iter().map(|r| r.name).collect(), verdicts.len())
        }

        async fn put(&self, path: &str) {
            self.storage
                .put(path, bytes::Bytes::from_static(b"cached"))
                .await
                .unwrap();
        }

        /// A published version, dated by the caller's clock like every other
        /// row here.
        async fn publish(&self, repository: i64, package: &str, version: &str, at: DateTime<Utc>) {
            let tarball_path = format!("{package}/{version}.tgz");
            self.put(&tarball_path).await;
            self.packages
                .publish_version(&NewRelease {
                    repository,
                    package,
                    match_name: NameMatch::Exact,
                    description: None,
                    readme: None,
                    version,
                    metadata_json: "{}",
                    checksum_sha1: None,
                    checksum_sha256: None,
                    integrity: None,
                    size: 6,
                    tarball_path: &tarball_path,
                    dist_tags: &["latest".to_string()],
                    now: at,
                })
                .await
                .unwrap();
        }

        /// What a package still holds: its versions, and the tags pointing at
        /// them, which the delete cascade takes with it.
        async fn left(&self, repository: i64, package: &str) -> (usize, usize) {
            let Some(found) = self
                .packages
                .package(repository, package, NameMatch::Exact)
                .await
                .unwrap()
            else {
                return (0, 0);
            };
            (
                self.packages.versions(found.id).await.unwrap().len(),
                self.packages.dist_tags(found.id).await.unwrap().len(),
            )
        }

        /// A row as it would have been written at `at`: what the caller's
        /// clock puts in `fetched_at`, `last_used_at` and the expiry.
        async fn entry(
            &self,
            kind: &str,
            key: &str,
            status: i64,
            ttl: Option<u64>,
            at: DateTime<Utc>,
        ) {
            let path = format!("_proxy_cache/p/{kind}/{key}");
            if status == 200 {
                self.put(&path).await;
            }
            self.cache
                .upsert(
                    &NewEntry {
                        repository_id: self.proxy,
                        kind,
                        cache_key: key,
                        status,
                        storage_path: (status == 200).then_some(path.as_str()),
                        content_type: None,
                        etag: None,
                        digest: None,
                        size: 6,
                        ttl_secs: ttl,
                    },
                    at,
                )
                .await
                .unwrap();
        }

        /// Whether the cache still holds a key, fresh or stale.
        async fn cached(&self, kind: &str, key: &str) -> bool {
            self.cache
                .entry(self.proxy, kind, key, now())
                .await
                .unwrap()
                .is_some()
        }
    }

    /// The pre-release sweep must delete npm/cargo pre-releases but spare Go
    /// pseudo-versions (which always carry a '-' yet are permanent artifacts).
    #[tokio::test]
    async fn cleanup_spares_go_pseudo_versions() {
        let fx = fixture().await;
        fx.publish(fx.npm, "npmpkg", "1.0.0-beta", days_ago(10)).await;
        fx.publish(fx.go, "gomod", "v0.0.0-20200101000000-abcdef", days_ago(10))
            .await;

        let deleted = sweep_prereleases(fx.packages.as_ref(), &fx.storage, 0, now())
            .await
            .unwrap();

        assert_eq!(deleted, 1);
        assert_eq!(
            fx.left(fx.npm, "npmpkg").await,
            (0, 0),
            "the npm pre-release goes, and its dist-tag with it"
        );
        assert!(
            !fx.storage.exists("npmpkg/1.0.0-beta.tgz").await.unwrap(),
            "the artifact goes before the row that names it"
        );
        assert_eq!(
            fx.left(fx.go, "gomod").await,
            (1, 1),
            "go pseudo-version must be spared (cross-format data-loss guard)"
        );
    }

    /// A version still inside its retention is not swept, and the bound is
    /// read against the caller's clock, never a store's.
    #[tokio::test]
    async fn a_prerelease_inside_its_retention_stays() {
        let fx = fixture().await;
        fx.publish(fx.npm, "npmpkg", "1.0.0-beta", days_ago(29)).await;

        let deleted = sweep_prereleases(fx.packages.as_ref(), &fx.storage, 30, now())
            .await
            .unwrap();

        assert_eq!(deleted, 0);
        assert_eq!(fx.left(fx.npm, "npmpkg").await, (1, 1));
    }

    #[tokio::test]
    async fn sweep_evicts_idle_immutable_and_expired_negatives_keeps_stale_positive() {
        let fx = fixture().await;
        fx.entry("npm-tarball", "idle", 200, None, days_ago(31)).await;
        fx.entry("npm-metadata", "gone", 404, Some(60), days_ago(1))
            .await;
        fx.entry("npm-metadata", "stale", 200, Some(60), days_ago(1))
            .await;
        fx.entry("npm-metadata", "fresh-negative", 404, Some(60), now())
            .await;

        let stats = sweep_proxy_cache(fx.cache.as_ref(), &fx.storage, 30, now())
            .await
            .unwrap();

        assert_eq!(
            stats,
            SweepStats {
                rows: 2,
                files: 1,
                parts: 0
            }
        );
        assert!(!fx.cached("npm-tarball", "idle").await);
        assert!(!fx.cached("npm-metadata", "gone").await);
        assert!(fx.cached("npm-metadata", "stale").await);
        assert!(fx.cached("npm-metadata", "fresh-negative").await);
        assert!(!fx.storage.exists("_proxy_cache/p/npm-tarball/idle").await.unwrap());
        assert!(fx.storage.exists("_proxy_cache/p/npm-metadata/stale").await.unwrap());
    }

    #[tokio::test]
    async fn sweep_reclaims_abandoned_part_file() {
        let fx = fixture().await;
        fx.put("_proxy_cache/p/npm-tarball/ab/abc.part-old").await;
        fx.put("_proxy_cache/p/npm-tarball/ab/abc.part-new").await;
        fx.put("_proxy_cache/p/npm-tarball/ab/abc").await;
        let old = fx.storage.resolve("_proxy_cache/p/npm-tarball/ab/abc.part-old").unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();

        let stats = sweep_proxy_cache(fx.cache.as_ref(), &fx.storage, 30, now())
            .await
            .unwrap();

        assert_eq!(stats.parts, 1);
        assert!(!old.exists(), "an hour-old part is reclaimed");
        assert!(fx.storage.exists("_proxy_cache/p/npm-tarball/ab/abc.part-new").await.unwrap());
        assert!(fx.storage.exists("_proxy_cache/p/npm-tarball/ab/abc").await.unwrap());
    }

    #[tokio::test]
    async fn run_cleanup_with_enabled_false_still_sweeps_proxy() {
        let fx = fixture().await;
        fx.entry("npm-tarball", "idle", 200, None, days_ago(31)).await;
        fx.publish(fx.npm, "npmpkg", "1.0.0-beta", days_ago(10)).await;
        let config = CleanupConfig {
            enabled: false,
            prerelease_older_than_days: Some(0),
            proxy_cache_older_than_days: Some(30),
            policy_report_older_than_days: None,
        };

        let stats = fx.sweeps().run(&config, now()).await;

        assert_eq!(stats.prereleases, None, "pre-release sweep needs enabled");
        assert_eq!(stats.proxy.map(|s| s.rows), Some(1));
        assert!(!fx.cached("npm-tarball", "idle").await);
        assert_eq!(fx.left(fx.npm, "npmpkg").await, (1, 1));

        let disabled = CleanupConfig {
            proxy_cache_older_than_days: Some(0),
            ..config
        };
        let stats = fx.sweeps().run(&disabled, now()).await;
        assert!(stats.prereleases.is_none() && stats.proxy.is_none() && stats.policy.is_none());
    }

    #[test]
    fn sweeps_configured_by_policy_bound_alone() {
        let policy_only = CleanupConfig {
            enabled: false,
            prerelease_older_than_days: None,
            proxy_cache_older_than_days: Some(0),
            policy_report_older_than_days: Some(30),
        };
        assert!(sweeps_configured(&policy_only));
        for bound in [Some(0), None] {
            let nothing = CleanupConfig {
                policy_report_older_than_days: bound,
                ..policy_only.clone()
            };
            assert!(!sweeps_configured(&nothing), "{bound:?}");
        }
        assert!(
            sweeps_configured(&CleanupConfig::default()),
            "the 90-day default sweeps"
        );
    }

    #[tokio::test]
    async fn policy_sweep_deletes_old_rows() {
        let fx = fixture().await;
        fx.policy_row("old-a", days_ago(100)).await;
        fx.policy_row("old-b", days_ago(31)).await;
        fx.policy_row("recent", days_ago(29)).await;
        fx.policy_row("today", now()).await;
        assert_eq!(fx.policy_left().await.1, 4);
        let config = CleanupConfig {
            enabled: false,
            prerelease_older_than_days: None,
            proxy_cache_older_than_days: Some(0),
            policy_report_older_than_days: Some(30),
        };

        let stats = fx.sweeps().run(&config, now()).await;

        assert_eq!(stats.policy, Some(2));
        assert!(stats.prereleases.is_none() && stats.proxy.is_none());
        assert_eq!(
            fx.policy_left().await,
            (vec!["today".to_string(), "recent".to_string()], 2),
            "the two rows inside retention stay, and their verdicts with them"
        );

        let off = CleanupConfig {
            policy_report_older_than_days: Some(0),
            ..config
        };
        let stats = fx.sweeps().run(&off, now()).await;
        assert_eq!(stats.policy, None);
        assert_eq!(fx.policy_left().await.0.len(), 2);
    }
}
