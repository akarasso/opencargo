use std::str::FromStr;

use chrono::Duration;
use sqlx::sqlite::SqliteConnectOptions;

use super::*;
use crate::domain::{RuleVerdict, Verdict};
use crate::testing::fixture::Fx;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-17T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// The fixture's database with foreign keys off: a verdict left behind by a
/// cascade shows.
async fn without_cascade(fx: &Fx) -> SqlitePolicyStore {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}", fx.db_path().display()))
        .unwrap()
        .foreign_keys(false);
    SqlitePolicyStore::new(SqlitePool::connect_with(opts).await.unwrap())
}

/// A pool of this fixture's own over the same file, foreign keys on.
async fn pool(fx: &Fx) -> SqlitePool {
    crate::adapters::sqlite::connect(&format!("sqlite:{}", fx.db_path().display()))
        .await
        .unwrap()
}

fn verdicts() -> Vec<RuleVerdict> {
    vec![RuleVerdict::new("typosquat", Verdict::Pass, "")]
}

async fn rows(store: &SqlitePolicyStore, n: i64, at: DateTime<Utc>, user_id: i64) {
    let verdicts = verdicts();
    let row = NewResolution {
        requested_repo: "p",
        member_repo: "p",
        format: "npm",
        name: "widget",
        version: None,
        digest: None,
        published_at: None,
        date_source: "",
        actor: "ci",
        actor_kind: "token",
        user_id: Some(user_id),
        verdicts: &verdicts,
    };
    let batch: Vec<NewResolution<'_>> = (0..n)
        .map(|_| NewResolution {
            verdicts: &verdicts,
            ..row
        })
        .collect();
    store.insert_batch(&batch, at).await.unwrap();
}

async fn counts(store: &SqlitePolicyStore) -> (i64, i64) {
    let resolutions = sqlx::query_scalar("SELECT COUNT(*) FROM policy_resolutions")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    let verdicts = sqlx::query_scalar("SELECT COUNT(*) FROM policy_verdicts")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    (resolutions, verdicts)
}

fn window(since: DateTime<Utc>) -> ReportFilter<'static> {
    ReportFilter {
        since,
        repo: None,
        rule: None,
        subject: None,
    }
}

/// The clause the `now` parameter of every writing method exists for: what
/// the row carries is what the caller passed, never the server's clock and
/// never the column's `DEFAULT (datetime('now'))`.
#[tokio::test]
async fn a_batch_carries_the_callers_clock_not_the_column_default() {
    let fx = Fx::new().await;
    let store = SqlitePolicyStore::new(pool(&fx).await);
    let at = now() - Duration::days(400);
    rows(&store, 1, at, 1).await;

    let listed = store.resolutions(&window(at), 1, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].created_at, at);
}

/// The retention cut-off is the caller's, so a row written in the past goes
/// and a recent one stays without the database ever reading its own clock.
#[tokio::test]
async fn retention_deletes_chunk_by_chunk_with_verdicts_explicit() {
    let fx = Fx::new().await;
    let store = without_cascade(&fx).await;
    rows(&store, DELETE_CHUNK + 1, now() - Duration::days(100), 1).await;
    rows(&store, 2, now() - Duration::days(1), 1).await;
    assert_eq!(counts(&store).await, (DELETE_CHUNK + 3, DELETE_CHUNK + 3));

    let deleted = store.delete_older_than(30, now()).await.unwrap();

    assert_eq!(
        deleted as i64,
        DELETE_CHUNK + 1,
        "a second chunk finished the job"
    );
    assert_eq!(
        counts(&store).await,
        (2, 2),
        "verdicts went with their rows, no cascade needed"
    );
    assert_eq!(store.delete_older_than(30, now()).await.unwrap(), 0);
}

#[tokio::test]
async fn erasure_deletes_chunk_by_chunk_with_verdicts_explicit() {
    let fx = Fx::new().await;
    let store = without_cascade(&fx).await;
    rows(&store, DELETE_CHUNK + 1, now(), 7).await;
    rows(&store, 1, now(), 8).await;

    let deleted = store.erase_user(7).await.unwrap();

    assert_eq!(deleted as i64, DELETE_CHUNK + 1);
    assert_eq!(counts(&store).await, (1, 1));
    let left: i64 = sqlx::query_scalar("SELECT user_id FROM policy_resolutions")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(left, 8);
}
