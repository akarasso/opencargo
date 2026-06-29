use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::config::CleanupConfig;
use crate::storage::StorageBackend;

/// Start a background cleanup task that runs every 24 hours.
///
/// The task cleans up:
/// - Pre-release versions older than the configured number of days
/// - Proxy cache entries older than the configured number of days
pub async fn start_cleanup_task(
    db: SqlitePool,
    storage: Arc<dyn StorageBackend>,
    config: CleanupConfig,
) {
    if !config.enabled {
        info!("Cleanup task is disabled");
        return;
    }

    info!("Cleanup task started (runs every 24h)");
    loop {
        tokio::time::sleep(Duration::from_secs(86400)).await;
        run_cleanup(&db, &storage, &config).await;
    }
}

async fn run_cleanup(
    db: &SqlitePool,
    storage: &Arc<dyn StorageBackend>,
    config: &CleanupConfig,
) {
    info!("Running scheduled cleanup");

    if let Some(days) = config.prerelease_older_than_days {
        if let Err(e) = cleanup_old_prereleases(db, storage, days).await {
            error!(error = %e, "Failed to clean up old pre-release versions");
        }
    }

    if let Some(days) = config.proxy_cache_older_than_days {
        if let Err(e) = cleanup_proxy_cache(db, days).await {
            error!(error = %e, "Failed to clean up proxy cache entries");
        }
    }
}

/// Delete pre-release versions (versions containing '-') that were published
/// more than `older_than_days` days ago.
async fn cleanup_old_prereleases(
    db: &SqlitePool,
    storage: &Arc<dyn StorageBackend>,
    older_than_days: u64,
) -> anyhow::Result<()> {
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
        return Ok(());
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

    Ok(())
}

/// Delete proxy cache metadata entries that were fetched more than
/// `older_than_days` days ago.
async fn cleanup_proxy_cache(db: &SqlitePool, older_than_days: u64) -> anyhow::Result<()> {
    let result = sqlx::query(
        "DELETE FROM proxy_cache_meta
         WHERE datetime(fetched_at, '+' || ?1 || ' days') < datetime('now')",
    )
    .bind(older_than_days as i64)
    .execute(db)
    .await?;

    let deleted = result.rows_affected();
    info!(
        deleted_entries = deleted,
        "Proxy cache cleanup complete"
    );

    Ok(())
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
    use crate::storage::FilesystemStorage;

    /// The pre-release sweep must delete npm/cargo pre-releases but spare Go
    /// pseudo-versions (which always carry a '-' yet are permanent artifacts).
    #[tokio::test]
    async fn cleanup_spares_go_pseudo_versions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let url = format!("sqlite:{}?mode=rwc", db_path.display());
        let pool = SqlitePool::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO repositories (name, repo_type, format) VALUES
             ('npmrepo','hosted','npm'), ('gorepo','hosted','go')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO packages (repository_id, name) VALUES (1,'npmpkg'), (2,'gomod')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO versions (package_id, version, metadata_json, tarball_path, published_at) VALUES
             (1,'1.0.0-beta','{}','npm/npmrepo/npmpkg/x.tgz', datetime('now','-10 days')),
             (2,'v0.0.0-20200101000000-abcdef','{}','go/gorepo/gomod/x.zip', datetime('now','-10 days'))",
        )
        .execute(&pool)
        .await
        .unwrap();

        let storage: Arc<dyn StorageBackend> =
            Arc::new(FilesystemStorage::new(tmp.path().join("storage")));

        cleanup_old_prereleases(&pool, &storage, 0).await.unwrap();

        let npm_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM versions WHERE version = '1.0.0-beta'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let go_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM versions WHERE version LIKE 'v0.0.0-%'")
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(npm_left, 0, "npm pre-release should be cleaned up");
        assert_eq!(
            go_left, 1,
            "go pseudo-version must be spared (cross-format data-loss guard)"
        );
    }
}
