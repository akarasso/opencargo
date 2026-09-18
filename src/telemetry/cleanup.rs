use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::config::CleanupConfig;
use crate::db::proxy_cache;
use crate::storage::StorageBackend;

/// Abandoned `*.part-*` files older than this are reclaimed by the sweep.
const STALE_PART_AGE: Duration = Duration::from_secs(3600);

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
    db: SqlitePool,
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
        run_cleanup(&db, &storage, &config).await;
        tokio::time::sleep(Duration::from_secs(86400)).await;
    }
}

pub(crate) async fn run_cleanup(
    db: &SqlitePool,
    storage: &Arc<dyn StorageBackend>,
    config: &CleanupConfig,
) -> CleanupStats {
    info!("Running scheduled cleanup");
    let mut stats = CleanupStats::default();

    if let Some(days) = config.prerelease_older_than_days.filter(|_| config.enabled) {
        match cleanup_old_prereleases(db, storage, days).await {
            Ok(deleted) => stats.prereleases = Some(deleted),
            Err(e) => error!(error = %e, "Failed to clean up old pre-release versions"),
        }
    }

    if let Some(days) = proxy_idle_days(config) {
        match sweep_proxy_cache(db, storage, days).await {
            Ok(sweep) => stats.proxy = Some(sweep),
            Err(e) => error!(error = %e, "Failed to sweep the proxy cache"),
        }
    }

    if let Some(days) = policy_days(config) {
        match crate::policy::store::delete_older_than(db, days).await {
            Ok(deleted) => {
                info!(deleted, days, "Policy report sweep complete");
                stats.policy = Some(deleted);
            }
            Err(e) => error!(error = %e, "Failed to sweep the policy report"),
        }
    }
    stats
}

/// Delete pre-release versions (versions containing '-') that were published
/// more than `older_than_days` days ago; returns how many went.
async fn cleanup_old_prereleases(
    db: &SqlitePool,
    storage: &Arc<dyn StorageBackend>,
    older_than_days: u64,
) -> anyhow::Result<u64> {
    // Find pre-release versions older than the threshold. In semver a '-'
    // introduces a pre-release (e.g. 1.0.0-beta.1), but that only holds for
    // formats whose versions ARE semver. Go pseudo-versions
    // (v0.0.0-20210101000000-abcdef) and OCI tags (v1.2.3-amd64) routinely
    // contain '-' while being permanent, legitimate artifacts — deleting them
    // would be silent data loss. Restrict the sweep to npm/cargo, where a '-'
    // genuinely marks a discardable pre-release.
    let rows = sqlx::query_as::<_, PrereleaseRow>(
        "SELECT v.id, v.version, v.tarball_path, p.name AS package_name
         FROM versions v
         JOIN packages p ON p.id = v.package_id
         JOIN repositories r ON r.id = p.repository_id
         WHERE r.format IN ('npm', 'cargo')
           AND v.version LIKE '%-%'
           AND datetime(v.published_at, '+' || ?1 || ' days') < datetime('now')",
    )
    .bind(older_than_days as i64)
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        info!("No old pre-release versions to clean up");
        return Ok(0);
    }

    info!(count = rows.len(), "Cleaning up old pre-release versions");

    for row in &rows {
        // Delete the tarball from storage
        if let Err(e) = storage.delete(&row.tarball_path).await {
            warn!(
                version_id = row.id,
                path = %row.tarball_path,
                error = %e,
                "Failed to delete tarball for pre-release version"
            );
        }

        // Delete associated dist tags
        sqlx::query("DELETE FROM dist_tags WHERE version_id = ?1")
            .bind(row.id)
            .execute(db)
            .await?;

        // Delete associated download records: the legacy per-row table (still
        // needed so the versions FK can be removed) AND the aggregate counter.
        sqlx::query("DELETE FROM downloads WHERE version_id = ?1")
            .bind(row.id)
            .execute(db)
            .await?;
        sqlx::query("DELETE FROM download_counts WHERE version_id = ?1")
            .bind(row.id)
            .execute(db)
            .await?;

        // Delete the version row
        sqlx::query("DELETE FROM versions WHERE id = ?1")
            .bind(row.id)
            .execute(db)
            .await?;

        info!(
            package = %row.package_name,
            version = %row.version,
            "Deleted old pre-release version"
        );
    }

    Ok(rows.len() as u64)
}

/// Evict expired negative entries and every row idle for `idle_days`, file
/// first, then row; then reclaim abandoned part files.
pub(crate) async fn sweep_proxy_cache(
    db: &SqlitePool,
    storage: &Arc<dyn StorageBackend>,
    idle_days: u64,
) -> anyhow::Result<SweepStats> {
    let mut stats = SweepStats::default();
    for row in proxy_cache::evictable_entries(db, idle_days).await? {
        if let Some(path) = &row.storage_path {
            match storage.delete(path).await {
                Ok(()) => stats.files += 1,
                Err(e) => warn!(path, error = %e, "Failed to delete an evicted cache file"),
            }
        }
        sqlx::query("DELETE FROM proxy_cache_entries WHERE id = ?1")
            .bind(row.id)
            .execute(db)
            .await?;
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

#[derive(Debug, sqlx::FromRow)]
struct PrereleaseRow {
    id: i64,
    version: String,
    tarball_path: String,
    package_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::proxy_cache::NewEntry;

    struct Fx {
        _tmp: tempfile::TempDir,
        pool: SqlitePool,
        storage: Arc<dyn StorageBackend>,
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::server::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO repositories (name, repo_type, format, upstream_url) VALUES
             ('npmrepo','hosted','npm',NULL), ('gorepo','hosted','go',NULL),
             ('p','proxy','npm','https://registry.npmjs.org')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let storage = crate::storage::filesystem(tmp.path().join("storage"));
        Fx {
            _tmp: tmp,
            pool,
            storage,
        }
    }

    impl Fx {
        async fn put(&self, path: &str) {
            self.storage
                .put(path, bytes::Bytes::from_static(b"cached"))
                .await
                .unwrap();
        }

        async fn entry(&self, kind: &str, key: &str, status: i64, ttl: Option<u64>) -> i64 {
            let path = format!("_proxy_cache/p/{kind}/{key}");
            if status == 200 {
                self.put(&path).await;
            }
            proxy_cache::upsert_entry(
                &self.pool,
                &NewEntry {
                    repository_id: 3,
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
            )
            .await
            .unwrap();
            proxy_cache::get_entry(&self.pool, 3, kind, key)
                .await
                .unwrap()
                .unwrap()
                .0
                .id
        }

        async fn set(&self, id: i64, column: &str, value: &str) {
            sqlx::query(&format!(
                "UPDATE proxy_cache_entries SET {column} = {value} WHERE id = ?1"
            ))
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn keys(&self) -> Vec<String> {
            sqlx::query_scalar("SELECT cache_key FROM proxy_cache_entries ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }
    }

    /// The pre-release sweep must delete npm/cargo pre-releases but spare Go
    /// pseudo-versions (which always carry a '-' yet are permanent artifacts).
    #[tokio::test]
    async fn cleanup_spares_go_pseudo_versions() {
        let fx = fixture().await;
        sqlx::query("INSERT INTO packages (repository_id, name) VALUES (1,'npmpkg'), (2,'gomod')")
            .execute(&fx.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO versions (package_id, version, metadata_json, tarball_path, published_at) VALUES
             (1,'1.0.0-beta','{}','npm/npmrepo/npmpkg/x.tgz', datetime('now','-10 days')),
             (2,'v0.0.0-20200101000000-abcdef','{}','go/gorepo/gomod/x.zip', datetime('now','-10 days'))",
        )
        .execute(&fx.pool)
        .await
        .unwrap();

        let deleted = cleanup_old_prereleases(&fx.pool, &fx.storage, 0).await.unwrap();
        assert_eq!(deleted, 1);

        let npm_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM versions WHERE version = '1.0.0-beta'")
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        let go_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM versions WHERE version LIKE 'v0.0.0-%'")
                .fetch_one(&fx.pool)
                .await
                .unwrap();

        assert_eq!(npm_left, 0, "npm pre-release should be cleaned up");
        assert_eq!(
            go_left, 1,
            "go pseudo-version must be spared (cross-format data-loss guard)"
        );
    }

    #[tokio::test]
    async fn sweep_evicts_idle_immutable_and_expired_negatives_keeps_stale_positive() {
        let fx = fixture().await;
        let idle = fx.entry("npm-tarball", "idle", 200, None).await;
        fx.set(idle, "last_used_at", "datetime('now', '-31 days')").await;
        let negative = fx.entry("npm-metadata", "gone", 404, Some(60)).await;
        fx.set(negative, "expires_at", "datetime('now', '-1 second')").await;
        let stale = fx.entry("npm-metadata", "stale", 200, Some(60)).await;
        fx.set(stale, "expires_at", "datetime('now', '-1 second')").await;
        fx.entry("npm-metadata", "fresh-negative", 404, Some(60)).await;

        let stats = sweep_proxy_cache(&fx.pool, &fx.storage, 30).await.unwrap();

        assert_eq!(
            stats,
            SweepStats {
                rows: 2,
                files: 1,
                parts: 0
            }
        );
        assert_eq!(fx.keys().await, vec!["stale", "fresh-negative"]);
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

        let stats = sweep_proxy_cache(&fx.pool, &fx.storage, 30).await.unwrap();

        assert_eq!(stats.parts, 1);
        assert!(!old.exists(), "an hour-old part is reclaimed");
        assert!(fx.storage.exists("_proxy_cache/p/npm-tarball/ab/abc.part-new").await.unwrap());
        assert!(fx.storage.exists("_proxy_cache/p/npm-tarball/ab/abc").await.unwrap());
    }

    #[tokio::test]
    async fn run_cleanup_with_enabled_false_still_sweeps_proxy() {
        let fx = fixture().await;
        let idle = fx.entry("npm-tarball", "idle", 200, None).await;
        fx.set(idle, "last_used_at", "datetime('now', '-31 days')").await;
        sqlx::query("INSERT INTO packages (repository_id, name) VALUES (1,'npmpkg')")
            .execute(&fx.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO versions (package_id, version, metadata_json, tarball_path, published_at)
             VALUES (1,'1.0.0-beta','{}','npm/npmrepo/npmpkg/x.tgz', datetime('now','-10 days'))",
        )
        .execute(&fx.pool)
        .await
        .unwrap();
        let config = CleanupConfig {
            enabled: false,
            prerelease_older_than_days: Some(0),
            proxy_cache_older_than_days: Some(30),
            policy_report_older_than_days: None,
        };

        let stats = run_cleanup(&fx.pool, &fx.storage, &config).await;

        assert_eq!(stats.prereleases, None, "pre-release sweep needs enabled");
        assert_eq!(stats.proxy.map(|s| s.rows), Some(1));
        assert!(fx.keys().await.is_empty());
        let versions_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM versions")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert_eq!(versions_left, 1);

        let disabled = CleanupConfig {
            proxy_cache_older_than_days: Some(0),
            ..config
        };
        let stats = run_cleanup(&fx.pool, &fx.storage, &disabled).await;
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

    impl Fx {
        async fn policy_row(&self, name: &str, days_ago: i64) -> i64 {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO policy_resolutions (created_at, requested_repo, member_repo, format, name, actor, actor_kind)
                 VALUES (datetime('now', ?1 || ' days'), 'p', 'p', 'npm', ?2, 'anonymous', 'anonymous') RETURNING id",
            )
            .bind(format!("-{days_ago}"))
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO policy_verdicts (resolution_id, rule, verdict) VALUES (?1, 'typosquat', 'pass')",
            )
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
            id
        }

        async fn policy_names(&self) -> Vec<String> {
            sqlx::query_scalar("SELECT name FROM policy_resolutions ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }

        async fn verdict_count(&self) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM policy_verdicts")
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn policy_sweep_deletes_old_rows() {
        let fx = fixture().await;
        fx.policy_row("old-a", 100).await;
        fx.policy_row("old-b", 31).await;
        fx.policy_row("recent", 29).await;
        fx.policy_row("today", 0).await;
        assert_eq!(fx.verdict_count().await, 4);
        let config = CleanupConfig {
            enabled: false,
            prerelease_older_than_days: None,
            proxy_cache_older_than_days: Some(0),
            policy_report_older_than_days: Some(30),
        };

        let stats = run_cleanup(&fx.pool, &fx.storage, &config).await;

        assert_eq!(stats.policy, Some(2));
        assert!(stats.prereleases.is_none() && stats.proxy.is_none());
        assert_eq!(fx.policy_names().await, vec!["recent", "today"]);
        assert_eq!(fx.verdict_count().await, 2, "verdicts cascade");

        let off = CleanupConfig {
            policy_report_older_than_days: Some(0),
            ..config
        };
        let stats = run_cleanup(&fx.pool, &fx.storage, &off).await;
        assert_eq!(stats.policy, None);
        assert_eq!(fx.policy_names().await.len(), 2);
    }
}
