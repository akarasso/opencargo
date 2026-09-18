use std::str::FromStr;

use sqlx::sqlite::SqliteConnectOptions;

use super::*;
use crate::testing::fixture::Fx;

/// The fixture's database with foreign keys off: a verdict left
/// behind by a cascade shows.
async fn without_cascade(fx: &Fx) -> SqlitePool {
    let file: String = sqlx::query_scalar("SELECT file FROM pragma_database_list")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{file}"))
        .unwrap()
        .foreign_keys(false);
    SqlitePool::connect_with(opts).await.unwrap()
}

async fn rows(pool: &SqlitePool, n: i64, days_ago: i64, user_id: i64) {
    let mut tx = pool.begin().await.unwrap();
    for _ in 0..n {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO policy_resolutions (created_at, requested_repo, member_repo, format, name, actor, actor_kind, user_id)
             VALUES (datetime('now', ?1 || ' days'), 'p', 'p', 'npm', 'widget', 'ci', 'token', ?2) RETURNING id",
        )
        .bind(format!("-{days_ago}"))
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO policy_verdicts (resolution_id, rule, verdict) VALUES (?1, 'typosquat', 'pass')")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

async fn counts(pool: &SqlitePool) -> (i64, i64) {
    let resolutions = sqlx::query_scalar("SELECT COUNT(*) FROM policy_resolutions")
        .fetch_one(pool)
        .await
        .unwrap();
    let verdicts = sqlx::query_scalar("SELECT COUNT(*) FROM policy_verdicts")
        .fetch_one(pool)
        .await
        .unwrap();
    (resolutions, verdicts)
}

#[tokio::test]
async fn retention_deletes_chunk_by_chunk_with_verdicts_explicit() {
    let fx = Fx::new().await;
    let pool = without_cascade(&fx).await;
    rows(&pool, DELETE_CHUNK + 1, 100, 1).await;
    rows(&pool, 2, 1, 1).await;
    assert_eq!(counts(&pool).await, (DELETE_CHUNK + 3, DELETE_CHUNK + 3));

    let deleted = delete_older_than(&pool, 30).await.unwrap();

    assert_eq!(
        deleted as i64,
        DELETE_CHUNK + 1,
        "a second chunk finished the job"
    );
    assert_eq!(
        counts(&pool).await,
        (2, 2),
        "verdicts went with their rows, no cascade needed"
    );
    assert_eq!(delete_older_than(&pool, 30).await.unwrap(), 0);
}

#[tokio::test]
async fn erasure_deletes_chunk_by_chunk_with_verdicts_explicit() {
    let fx = Fx::new().await;
    let pool = without_cascade(&fx).await;
    rows(&pool, DELETE_CHUNK + 1, 0, 7).await;
    rows(&pool, 1, 0, 8).await;

    let deleted = delete_by_user(&pool, 7).await.unwrap();

    assert_eq!(deleted as i64, DELETE_CHUNK + 1);
    assert_eq!(counts(&pool).await, (1, 1));
    let left: i64 = sqlx::query_scalar("SELECT user_id FROM policy_resolutions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 8);
}
