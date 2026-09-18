//! What the sweep and the status tile will read, asserted against the real
//! schema: the clock is the caller's, and the threshold is a bound parameter.

use chrono::TimeZone;
use sqlx::SqlitePool;
use tempfile::TempDir;

use super::*;
use crate::adapters::sqlite::migrate;

const HOUR: Duration = Duration::from_secs(3600);

async fn ledger() -> (TempDir, SqlitePool, SqliteMultipartLedger) {
    let tmp = TempDir::new().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
    let pool = crate::adapters::sqlite::connect(&url).await.unwrap();
    migrate::run_all(&pool).await.unwrap();
    let ledger = SqliteMultipartLedger::new(pool.clone());
    (tmp, pool, ledger)
}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
}

#[tokio::test]
async fn open_and_close_move_the_in_flight_count() {
    let (_tmp, _pool, ledger) = ledger().await;
    assert_eq!(ledger.in_flight().await.unwrap(), 0);

    ledger.opened("u1", "npm/r/p/a.tgz", at(9)).await.unwrap();
    ledger.opened("u2", "npm/r/p/b.tgz", at(9)).await.unwrap();
    assert_eq!(ledger.in_flight().await.unwrap(), 2);

    ledger.closed("u1").await.unwrap();
    assert_eq!(ledger.in_flight().await.unwrap(), 1);
}

/// 1.5's contract clause: the row holds the timestamp the caller passed, so no
/// column default can fire behind the adapter's back.
#[tokio::test]
async fn a_row_carries_the_callers_clock_never_the_servers() {
    let (_tmp, pool, ledger) = ledger().await;
    let opened = at(9);
    ledger.opened("u1", "oci/r/p/sha256", opened).await.unwrap();

    let (started, touched): (String, String) =
        sqlx::query_as("SELECT started_at, touched_at FROM storage_multipart WHERE upload_id = ?1")
            .bind("u1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(started, bind_ts(opened));
    assert_eq!(touched, bind_ts(opened));

    let bumped = at(11);
    ledger.touched("u1", bumped).await.unwrap();
    let (started, touched): (String, String) =
        sqlx::query_as("SELECT started_at, touched_at FROM storage_multipart WHERE upload_id = ?1")
            .bind("u1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(started, bind_ts(opened), "a bump is not a restart");
    assert_eq!(touched, bind_ts(bumped));
}

#[tokio::test]
async fn idle_since_sees_only_uploads_untouched_for_the_age() {
    let (_tmp, _pool, ledger) = ledger().await;
    ledger.opened("old", "k/old", at(8)).await.unwrap();
    ledger.opened("older", "k/older", at(6)).await.unwrap();
    ledger.opened("fresh", "k/fresh", at(11)).await.unwrap();

    let abandoned = ledger.idle_since(HOUR, at(12)).await.unwrap();
    assert_eq!(
        abandoned,
        vec![
            ("older".to_string(), "k/older".to_string()),
            ("old".to_string(), "k/old".to_string()),
        ],
        "oldest first, and the fresh upload is not abandoned"
    );

    ledger.touched("older", at(12)).await.unwrap();
    let abandoned = ledger.idle_since(HOUR, at(12)).await.unwrap();
    assert_eq!(abandoned, vec![("old".to_string(), "k/old".to_string())]);
}

/// A writer's `Drop` and the sweep race for the same row; whoever loses must
/// not turn a collected upload into a 500.
#[tokio::test]
async fn closing_an_unknown_upload_is_not_an_error() {
    let (_tmp, _pool, ledger) = ledger().await;
    ledger.closed("never-opened").await.unwrap();
    ledger.touched("never-opened", at(9)).await.unwrap();
    assert_eq!(ledger.in_flight().await.unwrap(), 0);
}

/// Two writers cannot share an upload id: the second would make the first's
/// parts unreachable to the sweep.
#[tokio::test]
async fn reopening_an_upload_id_is_a_conflict() {
    let (_tmp, _pool, ledger) = ledger().await;
    ledger.opened("u1", "k/a", at(9)).await.unwrap();

    let err = ledger.opened("u1", "k/b", at(10)).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict), "{err}");
}
