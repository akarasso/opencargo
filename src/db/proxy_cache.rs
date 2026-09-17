use sqlx::{FromRow, Row, SqlitePool};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CacheEntry {
    pub id: i64,
    pub repository_id: i64,
    pub kind: String,
    pub cache_key: String,
    pub status: i64,
    pub storage_path: Option<String>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub digest: Option<String>,
    pub size: i64,
    pub fetched_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: String,
}

pub struct NewEntry<'a> {
    pub repository_id: i64,
    pub kind: &'a str,
    pub cache_key: &'a str,
    pub status: i64,
    pub storage_path: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub digest: Option<&'a str>,
    pub size: i64,
    pub ttl_secs: Option<u64>,
}

const FRESH: &str = "(expires_at IS NULL OR expires_at > datetime('now'))";

// datetime('now', NULL) is NULL, so a NULL ttl yields an immutable row.
const EXPIRY: &str = "datetime('now', '+' || ?1 || ' seconds')";

pub async fn get_entry(
    pool: &SqlitePool,
    repository_id: i64,
    kind: &str,
    key: &str,
) -> Result<Option<(CacheEntry, bool)>, sqlx::Error> {
    let sql = format!(
        "SELECT *, {FRESH} AS fresh FROM proxy_cache_entries
         WHERE repository_id = ?1 AND kind = ?2 AND cache_key = ?3"
    );
    sqlx::query(&sql)
        .bind(repository_id)
        .bind(kind)
        .bind(key)
        .fetch_optional(pool)
        .await?
        .map(|row| Ok((CacheEntry::from_row(&row)?, row.try_get("fresh")?)))
        .transpose()
}

pub async fn upsert_entry(pool: &SqlitePool, entry: &NewEntry<'_>) -> Result<(), sqlx::Error> {
    let sql = format!(
        "INSERT INTO proxy_cache_entries
             (repository_id, kind, cache_key, status, storage_path, content_type, etag, digest,
              size, fetched_at, expires_at, last_used_at)
         VALUES (?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, datetime('now'), {EXPIRY}, datetime('now'))
         ON CONFLICT(repository_id, kind, cache_key) DO UPDATE SET
             status = excluded.status, storage_path = excluded.storage_path,
             content_type = excluded.content_type, etag = excluded.etag,
             digest = excluded.digest, size = excluded.size, fetched_at = excluded.fetched_at,
             expires_at = excluded.expires_at, last_used_at = excluded.last_used_at"
    );
    sqlx::query(&sql)
        .bind(entry.ttl_secs.map(|s| s as i64))
        .bind(entry.repository_id)
        .bind(entry.kind)
        .bind(entry.cache_key)
        .bind(entry.status)
        .bind(entry.storage_path)
        .bind(entry.content_type)
        .bind(entry.etag)
        .bind(entry.digest)
        .bind(entry.size)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch_entry(
    pool: &SqlitePool,
    id: i64,
    extend_ttl_secs: Option<u64>,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE proxy_cache_entries
         SET last_used_at = datetime('now'), expires_at = COALESCE({EXPIRY}, expires_at)
         WHERE id = ?2"
    );
    sqlx::query(&sql)
        .bind(extend_ttl_secs.map(|s| s as i64))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_entries(pool: &SqlitePool, repository_id: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM proxy_cache_entries WHERE repository_id = ?1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn delete_legacy_meta(pool: &SqlitePool, repository_id: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM proxy_cache_meta WHERE repository_id = ?1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn evictable_entries(
    pool: &SqlitePool,
    idle_days: u64,
) -> Result<Vec<CacheEntry>, sqlx::Error> {
    sqlx::query_as::<_, CacheEntry>(
        "SELECT * FROM proxy_cache_entries
         WHERE (status <> 200 AND expires_at <= datetime('now'))
            OR last_used_at < datetime('now', '-' || ?1 || ' days')
         ORDER BY id",
    )
    .bind(idle_days as i64)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let (tmp, pool) = crate::db::testing::pool().await;
        sqlx::query("INSERT INTO repositories (name, repo_type, format, upstream_url) VALUES ('p', 'proxy', 'npm', 'https://registry.npmjs.org')")
            .execute(&pool)
            .await
            .unwrap();
        (tmp, pool)
    }

    fn entry<'a>(kind: &'a str, key: &'a str, status: i64, ttl_secs: Option<u64>) -> NewEntry<'a> {
        NewEntry {
            repository_id: 1,
            kind,
            cache_key: key,
            status,
            storage_path: (status == 200).then_some("_proxy_cache/p/x"),
            content_type: Some("application/json"),
            etag: Some("\"e1\""),
            digest: Some("abc"),
            size: 42,
            ttl_secs,
        }
    }

    async fn set(pool: &SqlitePool, id: i64, column: &str, value: &str) {
        sqlx::query(&format!(
            "UPDATE proxy_cache_entries SET {column} = {value} WHERE id = ?1"
        ))
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn fetch(pool: &SqlitePool, kind: &str, key: &str) -> (CacheEntry, bool) {
        get_entry(pool, 1, kind, key)
            .await
            .unwrap()
            .expect("row present")
    }

    #[tokio::test]
    async fn roundtrip_and_freshness() {
        let (_tmp, pool) = pool().await;
        assert!(get_entry(&pool, 1, "npm-metadata", "lodash")
            .await
            .unwrap()
            .is_none());

        upsert_entry(&pool, &entry("npm-metadata", "lodash", 200, Some(3600)))
            .await
            .unwrap();
        let (row, fresh) = fetch(&pool, "npm-metadata", "lodash").await;
        assert!(fresh);
        assert_eq!(
            (row.repository_id, row.kind.as_str(), row.cache_key.as_str()),
            (1, "npm-metadata", "lodash")
        );
        assert_eq!((row.status, row.size), (200, 42));
        assert_eq!(row.storage_path.as_deref(), Some("_proxy_cache/p/x"));
        assert_eq!(row.content_type.as_deref(), Some("application/json"));
        assert_eq!(row.etag.as_deref(), Some("\"e1\""));
        assert_eq!(row.digest.as_deref(), Some("abc"));
        assert!(row.expires_at.is_some());

        set(&pool, row.id, "expires_at", "datetime('now', '-1 second')").await;
        set(&pool, row.id, "last_used_at", "datetime('now', '-1 day')").await;
        let (stale, fresh) = fetch(&pool, "npm-metadata", "lodash").await;
        assert!(!fresh);

        touch_entry(&pool, row.id, None).await.unwrap();
        let (touched, fresh) = fetch(&pool, "npm-metadata", "lodash").await;
        assert!(!fresh, "a plain touch never extends the ttl");
        assert!(touched.last_used_at > stale.last_used_at);

        touch_entry(&pool, row.id, Some(600)).await.unwrap();
        let (revalidated, fresh) = fetch(&pool, "npm-metadata", "lodash").await;
        assert!(fresh, "a 304 extends the ttl");
        assert_eq!(revalidated.id, row.id);

        upsert_entry(&pool, &entry("npm-metadata", "lodash", 200, None))
            .await
            .unwrap();
        let (immutable, fresh) = fetch(&pool, "npm-metadata", "lodash").await;
        assert!(fresh);
        assert_eq!(immutable.id, row.id, "upsert updates the row in place");
        assert_eq!(immutable.expires_at, None);

        upsert_entry(&pool, &entry("npm-metadata", "nope", 404, Some(60)))
            .await
            .unwrap();
        let (negative, fresh) = fetch(&pool, "npm-metadata", "nope").await;
        assert!(fresh);
        assert_eq!((negative.status, negative.storage_path), (404, None));
        assert!(evictable_entries(&pool, 30).await.unwrap().is_empty());

        set(
            &pool,
            negative.id,
            "expires_at",
            "datetime('now', '-1 second')",
        )
        .await;
        set(&pool, row.id, "expires_at", "datetime('now', '-1 second')").await;
        let evictable = evictable_entries(&pool, 30).await.unwrap();
        assert_eq!(
            evictable.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![negative.id],
            "a stale positive row is kept"
        );

        set(&pool, row.id, "last_used_at", "datetime('now', '-31 days')").await;
        let evictable = evictable_entries(&pool, 30).await.unwrap();
        assert_eq!(
            evictable.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![row.id, negative.id]
        );

        sqlx::query("INSERT INTO proxy_cache_meta (repository_id, cache_key) VALUES (1, 'legacy')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(delete_legacy_meta(&pool, 1).await.unwrap(), 1);
        assert_eq!(delete_entries(&pool, 1).await.unwrap(), 2);
        assert_eq!(delete_entries(&pool, 1).await.unwrap(), 0);

        upsert_entry(
            &pool,
            &entry("npm-tarball", "lodash/lodash-1.tgz", 200, None),
        )
        .await
        .unwrap();
        sqlx::query("DELETE FROM repositories WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proxy_cache_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "ON DELETE CASCADE");
    }
}
