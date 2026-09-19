//! What every connection of the pool is opened with.

use super::*;

async fn pragma(pool: &SqlitePool, name: &str) -> i64 {
    sqlx::query_scalar(&format!("PRAGMA {name}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The journal mode, the checkpoint window and the size limit are one policy:
/// a commit is durable in the log, the database catches up once per window,
/// and the file is given back afterwards.
#[tokio::test]
async fn the_pool_opens_on_a_bounded_write_ahead_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pool = connect(&format!(
        "sqlite:{}?mode=rwc",
        tmp.path().join("pragmas.db").display()
    ))
    .await
    .unwrap();

    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(
        pragma(&pool, "wal_autocheckpoint").await.to_string(),
        WAL_AUTOCHECKPOINT_PAGES
    );
    assert_eq!(
        pragma(&pool, "journal_size_limit").await.to_string(),
        WAL_SIZE_LIMIT_BYTES
    );
    assert_eq!(pragma(&pool, "foreign_keys").await, 1);
}
