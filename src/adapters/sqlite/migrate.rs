//! The migrator: a version table, an ordered list of steps, and a baseline
//! that probes instead of assuming.
//!
//! The scheme it replaces was "re-run all fourteen files on every boot", which
//! only worked because twelve of them are `IF NOT EXISTS` and the two bare
//! `ALTER TABLE`s had their errors swallowed. Applying each file at most once
//! makes that swallow unnecessary, and makes a *partial* apply permanent — so
//! the sentinel that proves a file ran is the object it creates **last**, never
//! its first.

use std::future::Future;
use std::pin::Pin;

use chrono::Utc;
use sqlx::{Executor, Sqlite, SqliteConnection, SqlitePool};
use tracing::info;

use crate::error::StoreError;

const VERSION_TABLE: &str = "schema_migrations";

/// The table whose presence means "this database predates the migrator": it is
/// created by `001`, so a database that has it and no version table is one the
/// baseline has to probe.
const BASELINE_WITNESS: &str = "repositories";

/// The row that makes "this database predates the migrator" survive the version
/// table it is about to create. Inferring the baseline from that table's
/// absence loses the fact the moment the first id is recorded, so a run cut
/// short — a SIGTERM during a rollout, an eviction, an OOM kill — would come
/// back strict and die on `004`'s bare `ALTER TABLE`, on that boot and on every
/// boot after it.
const BASELINE_MARKER: &str = "baseline";

const CREATE_VERSION_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    id TEXT PRIMARY KEY,
    applied_at TEXT NOT NULL
)";

/// A migration's work: a file, or the Rust a table rebuild needs.
///
/// `Sql` covers every step in this tree today. `Rust` exists because the
/// `repositories` CHECK rebuild that `nuget.md` and `maven.md` both need is not
/// a file — it is `PRAGMA foreign_keys=OFF`, a transaction, a `sqlite_sequence`
/// restore and an orphan diff on one connection — and a file-only migrator
/// would leave it outside the version table, re-running on every boot.
#[derive(Clone, Copy)]
pub enum Step {
    Sql(&'static str),
    Rust(fn(&mut SqliteConnection) -> StepFuture<'_>),
}

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<(), StoreError>> + Send + 'a>>;

/// What proves a migration already ran on a database that predates the version
/// table. It is the **last** object the file creates: `sqlx::raw_sql` runs a
/// file with no enclosing transaction, so a first-object sentinel would adopt a
/// half-applied file and leave its tail missing forever.
#[derive(Clone, Copy)]
pub enum Sentinel {
    /// A row of `sqlite_master`: a table, an index or a trigger.
    Object(&'static str),
    /// A column added by a bare `ALTER TABLE`, which creates no object.
    Column {
        table: &'static str,
        column: &'static str,
    },
    /// The step creates nothing observable, so adoption cannot prove it ran and
    /// the baseline applies it. Only legal for an idempotent step.
    Unprovable,
}

#[derive(Clone, Copy)]
pub struct Migration {
    pub id: &'static str,
    pub sentinel: Sentinel,
    pub step: Step,
}

/// How a migration got into the version table: `Adopted` executed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Adopted,
    Applied,
}

macro_rules! sql_migration {
    ($id:literal, $file:literal, $sentinel:expr) => {
        Migration {
            id: $id,
            sentinel: $sentinel,
            step: Step::Sql(include_str!(concat!("migrations/", $file))),
        }
    };
}

/// Ids are keys: a shipped file is never renamed, renumbered or edited — the
/// README beside the files records every id with its checksum and
/// `scripts/boundary.sh` enforces it.
pub const MIGRATIONS: &[Migration] = &[
    sql_migration!("001", "001_initial.sql", Sentinel::Object("idx_downloads_version")),
    sql_migration!("002", "002_proxy_cache.sql", Sentinel::Object("proxy_cache_meta")),
    sql_migration!("003", "003_auth.sql", Sentinel::Object("idx_audit_log_created")),
    sql_migration!(
        "004",
        "004_cargo.sql",
        Sentinel::Column { table: "versions", column: "yanked" }
    ),
    sql_migration!(
        "005",
        "005_must_change_password.sql",
        Sentinel::Column { table: "users", column: "must_change_password" }
    ),
    sql_migration!("006", "006_oci.sql", Sentinel::Object("oci_uploads")),
    sql_migration!("007", "007_fts5.sql", Sentinel::Object("packages_fts_update")),
    sql_migration!("008", "008_deps.sql", Sentinel::Object("idx_package_deps_version")),
    sql_migration!("009", "009_vulns.sql", Sentinel::Object("idx_vuln_scans_version")),
    sql_migration!("010", "010_dynamic_config.sql", Sentinel::Object("webhooks")),
    sql_migration!("011", "011_download_counts.sql", Sentinel::Object("download_counts")),
    sql_migration!(
        "012",
        "012_oci_manifest_blobs.sql",
        Sentinel::Object("idx_oci_manifest_blobs_blob")
    ),
    sql_migration!(
        "013",
        "013_proxy_cache_entries.sql",
        Sentinel::Object("idx_proxy_cache_entries_last_used")
    ),
    sql_migration!("014", "014_policy.sql", Sentinel::Object("idx_policy_verdicts_res")),
    sql_migration!("015", "015_fts_rebuild.sql", Sentinel::Unprovable),
];

/// Bring a database up to date with every migration this binary carries.
pub async fn run_all(pool: &SqlitePool) -> Result<Vec<(&'static str, Outcome)>, StoreError> {
    run(pool, MIGRATIONS).await
}

/// Apply `migrations` in id order, exactly once each.
///
/// Three cases, and the baseline is what separates them:
/// 1. no version table, no `repositories` — a fresh database: apply everything;
/// 2. no version table but `repositories` present — the database predates the
///    migrator: **probe and adopt**, recording every id whose sentinel already
///    exists and applying every id whose sentinel does not. Not one
///    transaction: the probe is idempotent, so a crash mid-adopt is re-probed
///    on the next boot, which the baseline marker is what makes true;
/// 3. otherwise apply every id the version table does not hold.
pub async fn run(
    pool: &SqlitePool,
    migrations: &[Migration],
) -> Result<Vec<(&'static str, Outcome)>, StoreError> {
    let adopt = open_version_table(pool).await?;
    let recorded = applied(pool).await?;

    let mut ran = Vec::new();
    for migration in migrations {
        if recorded.iter().any(|id| id == migration.id) {
            continue;
        }
        let outcome = if adopt && probe(pool, &migration.sentinel).await? {
            Outcome::Adopted
        } else {
            apply(pool, &migration.step).await?;
            Outcome::Applied
        };
        record(pool, migration.id).await?;
        ran.push((migration.id, outcome));
    }
    if adopt {
        clear_baseline_marker(pool).await?;
    }

    let adopted = ran.iter().filter(|(_, o)| *o == Outcome::Adopted).count();
    info!(
        applied = ran.len() - adopted,
        adopted, "Database migrations applied"
    );
    Ok(ran)
}

/// Run one step on a connection of its own.
///
/// A `Rust` step owns per-connection state — `PRAGMA foreign_keys` is per
/// connection — so its connection is detached instead of returned to the pool,
/// or one request in five would silently skip the schema's cascades. A failed
/// step of either kind gets the same treatment: what it left behind is unknown.
async fn apply(pool: &SqlitePool, step: &Step) -> Result<(), StoreError> {
    let mut conn = pool.acquire().await.map_err(other)?;
    let result = match step {
        Step::Sql(sql) => sqlx::raw_sql(sql)
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(other),
        Step::Rust(step) => step(&mut conn).await,
    };
    if result.is_err() || matches!(step, Step::Rust(_)) {
        drop(conn.detach());
    }
    result
}

async fn probe(pool: &SqlitePool, sentinel: &Sentinel) -> Result<bool, StoreError> {
    match sentinel {
        Sentinel::Object(name) => has_object(pool, name).await,
        Sentinel::Column { table, column } => has_column(pool, table, column).await,
        Sentinel::Unprovable => Ok(false),
    }
}

async fn has_object(pool: &SqlitePool, name: &str) -> Result<bool, StoreError> {
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM sqlite_master WHERE name = ?1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(other)?;
    Ok(found.is_some())
}

async fn has_column(pool: &SqlitePool, table: &str, column: &str) -> Result<bool, StoreError> {
    let found: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")
            .bind(table)
            .bind(column)
            .fetch_optional(pool)
            .await
            .map_err(other)?;
    Ok(found.is_some())
}

/// Create the version table, and answer whether this run is a baseline.
///
/// The marker is written in the same transaction that creates the table and
/// removed only once the loop has recorded every id, so "predates the migrator"
/// is a durable fact rather than an inference from a table that the first
/// recorded id destroys.
async fn open_version_table(pool: &SqlitePool) -> Result<bool, StoreError> {
    let baselining =
        !has_object(pool, VERSION_TABLE).await? && has_object(pool, BASELINE_WITNESS).await?;

    let mut tx = pool.begin().await.map_err(other)?;
    sqlx::query(CREATE_VERSION_TABLE)
        .execute(&mut *tx)
        .await
        .map_err(other)?;
    if baselining {
        record(&mut *tx, BASELINE_MARKER).await?;
    }
    tx.commit().await.map_err(other)?;

    has_id(pool, BASELINE_MARKER).await
}

async fn applied(pool: &SqlitePool) -> Result<Vec<String>, StoreError> {
    sqlx::query_scalar("SELECT id FROM schema_migrations WHERE id <> ?1")
        .bind(BASELINE_MARKER)
        .fetch_all(pool)
        .await
        .map_err(other)
}

async fn has_id(pool: &SqlitePool, id: &str) -> Result<bool, StoreError> {
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM schema_migrations WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(other)?;
    Ok(found.is_some())
}

async fn clear_baseline_marker(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM schema_migrations WHERE id = ?1")
        .bind(BASELINE_MARKER)
        .execute(pool)
        .await
        .map_err(other)?;
    Ok(())
}

async fn record<'e, E: Executor<'e, Database = Sqlite>>(
    executor: E,
    id: &str,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO schema_migrations (id, applied_at) VALUES (?1, ?2)")
        .bind(id)
        // The adapter's stored timestamp format, second precision: the 23
        // columns defaulting to `datetime('now')` are written this way and are
        // compared lexicographically elsewhere.
        .bind(Utc::now().format("%Y-%m-%d %H:%M:%S").to_string())
        .execute(executor)
        .await
        .map_err(other)?;
    Ok(())
}

fn other(err: sqlx::Error) -> StoreError {
    StoreError::Other(Box::new(err))
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
