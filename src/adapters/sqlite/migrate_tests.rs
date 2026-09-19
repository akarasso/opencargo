//! The baseline is the part that bricks production when it is wrong, so every
//! shape a real database can be in gets a test: fresh, fully migrated by the
//! scheme this replaces, and stopped half-way at `012`.

use std::collections::BTreeSet;

use sqlx::SqlitePool;
use tempfile::TempDir;

use super::*;
use crate::domain::{Format, RepoKind};

/// A database of the 001-012 vintage, the shape `data/db/opencargo.db` is in:
/// `oci_manifest_blobs` present, `proxy_cache_entries` and the policy tables
/// absent. Committed as a dump of a run of 001-012, never copied from a live
/// cluster.
const SCHEMA_012: &str = include_str!("fixtures/schema_012.sql");

/// The two seeded packages, as the npm search endpoint would look them up.
const MATCH_SEEDED: &str =
    r#"SELECT COUNT(*) FROM packages_fts WHERE packages_fts MATCH '"left-pad" OR "is-odd"'"#;

async fn pool() -> (TempDir, SqlitePool) {
    let tmp = TempDir::new().unwrap();
    let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
    // The production pool: five connections, `foreign_keys` ON on each.
    let pool = crate::adapters::sqlite::connect(&url).await.unwrap();
    (tmp, pool)
}

/// The apply this migrator replaces: all fourteen files on every boot, with
/// `004`, `005` and `007` swallowing their errors. Every database older than
/// the version table was built by it, so the baseline has to face one.
async fn legacy_migrate(pool: &SqlitePool) {
    // The fourteen files that shipped before the version table; every id
    // above them postdates the scheme this stands in for.
    for migration in MIGRATIONS.iter().filter(|m| m.id <= "014") {
        let Step::Sql(sql) = migration.step else {
            unreachable!("no Rust step shipped before the migrator")
        };
        let result = sqlx::raw_sql(sql).execute(pool).await;
        if !matches!(migration.id, "004" | "005" | "007") {
            result.unwrap();
        }
    }
}

async fn objects(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT type || ' ' || name FROM sqlite_master ORDER BY type, name")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn insert_repository(
    pool: &SqlitePool,
    name: &str,
    repo_type: &str,
    format: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO repositories (name, repo_type, format) VALUES (?1, ?2, ?3)")
        .bind(name)
        .bind(repo_type)
        .bind(format)
        .execute(pool)
        .await
        .map(|_| ())
}

fn outcomes(ran: &[(&'static str, Outcome)], want: Outcome) -> Vec<&'static str> {
    ran.iter()
        .filter(|(_, got)| *got == want)
        .map(|(id, _)| *id)
        .collect()
}

fn ids(upto: usize) -> Vec<&'static str> {
    MIGRATIONS[..upto].iter().map(|m| m.id).collect()
}

/// The two bare `ALTER TABLE`s: a second strict apply adds the column again, so
/// "exactly one" is what proves the baseline held.
async fn assert_alters_ran_once(pool: &SqlitePool) {
    for (table, column) in [("versions", "yanked"), ("users", "must_change_password")] {
        let sql =
            format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{column}'");
        assert_eq!(count(pool, &sql).await, 1, "{table}.{column} added twice");
    }
}

/// (a) A database the old `migrate()` built adopts what it already has, runs
/// only the files it never saw, and ends with the object set of a fresh
/// database — the property that makes adoption safe at all.
#[tokio::test]
async fn a_fully_migrated_database_is_adopted_and_only_the_unseen_files_run() {
    let (_legacy_tmp, legacy) = pool().await;
    legacy_migrate(&legacy).await;

    let ran = run_all(&legacy).await.unwrap();
    assert_eq!(outcomes(&ran, Outcome::Adopted), ids(14));
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["015", "017", "018", "019", "020", "021", "024", "025", "026"]);

    assert_alters_ran_once(&legacy).await;

    let (_fresh_tmp, fresh) = pool().await;
    run_all(&fresh).await.unwrap();
    assert_eq!(objects(&legacy).await, objects(&fresh).await);
}

/// (b) The version table is the point: a second run executes nothing.
#[tokio::test]
async fn a_second_run_against_a_fresh_database_executes_nothing() {
    let (_tmp, pool) = pool().await;

    let first = run_all(&pool).await.unwrap();
    assert_eq!(outcomes(&first, Outcome::Applied), ids(MIGRATIONS.len()));

    assert!(run_all(&pool).await.unwrap().is_empty());
}

/// (c) The partially-migrated database blind baselining would brick: it must
/// gain `013` and `014`, and its packages — which reached no index, because
/// they predate the trigger that would have indexed them — must be searchable
/// afterwards.
#[tokio::test]
async fn a_012_database_gains_the_missing_files_and_a_populated_index() {
    let (_tmp, pool) = pool().await;
    sqlx::raw_sql(SCHEMA_012).execute(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO repositories (name, repo_type, format) VALUES ('npm-hosted', 'hosted', 'npm');
         INSERT INTO packages (repository_id, name) VALUES (1, 'left-pad'), (1, 'is-odd');
         INSERT INTO packages_fts(packages_fts) VALUES('delete-all');",
    )
    .execute(&pool)
    .await
    .unwrap();
    // `COUNT(*)` on an external-content table counts the content table, so the
    // index itself is only observable through a MATCH — which is how both
    // search call sites read it.
    assert_eq!(count(&pool, MATCH_SEEDED).await, 0);

    let ran = run_all(&pool).await.unwrap();
    assert_eq!(outcomes(&ran, Outcome::Adopted), ids(12));
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["013", "014", "015", "017", "018", "019", "020", "021", "024", "025", "026"]);

    assert_eq!(count(&pool, "SELECT COUNT(*) FROM proxy_cache_entries").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM policy_resolutions").await, 0);
    assert_eq!(count(&pool, MATCH_SEEDED).await, 2);
}

/// (e) The baseline that dies half-way — a SIGTERM during a rollout, an
/// eviction, an OOM kill, the usual way a first boot ends. Recording ids one at
/// a time destroys the version table's absence, which is the only thing that
/// said "this database predates the migrator", so without a durable marker the
/// next boot comes back strict and `004` fails with `duplicate column name:
/// yanked` — on that boot and on every boot after it.
#[tokio::test]
async fn an_interrupted_baseline_is_re_probed_on_the_next_boot() {
    let (_tmp, pool) = pool().await;
    legacy_migrate(&pool).await;

    let mut interrupted = MIGRATIONS[..3].to_vec();
    interrupted.push(Migration {
        id: "016",
        sentinel: Sentinel::Unprovable,
        step: Step::Rust(dies),
    });
    run(&pool, &interrupted).await.unwrap_err();
    assert_eq!(applied(&pool).await.unwrap(), ids(3));

    let ran = run_all(&pool).await.unwrap();
    assert_eq!(outcomes(&ran, Outcome::Adopted), ids(14)[3..].to_vec());
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["015", "017", "018", "019", "020", "021", "024", "025", "026"]);
    assert_alters_ran_once(&pool).await;
    // The marker is cleared by the run that finished the baseline, so the boot
    // after it is an ordinary strict one.
    assert!(run_all(&pool).await.unwrap().is_empty());
}

/// A step that fails without touching the database: the process dying is what
/// it stands in for.
fn dies(_: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(async { Err(StoreError::Other("the process was killed".into())) })
}

/// The sentinel rule itself: `sqlx::raw_sql` runs a file with no enclosing
/// transaction, so `014` can leave its first table behind and nothing else. A
/// first-object sentinel would adopt that database and its six indexes would
/// never exist.
#[tokio::test]
async fn a_half_applied_file_is_applied_not_adopted() {
    let (_tmp, pool) = pool().await;
    sqlx::raw_sql(SCHEMA_012).execute(&pool).await.unwrap();
    let policy = MIGRATIONS.iter().find(|m| m.id == "014").unwrap();
    let Step::Sql(sql) = policy.step else {
        unreachable!("014 is a file")
    };
    let first_statement = sql.split_inclusive(';').next().unwrap();
    sqlx::raw_sql(first_statement).execute(&pool).await.unwrap();

    let ran = run_all(&pool).await.unwrap();
    assert!(ran.contains(&("014", Outcome::Applied)));
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'idx_policy_verdicts_res'"
        )
        .await,
        1
    );
}

/// The columns `RepoKind` and `Format` round-trip through are constrained by a
/// CHECK the adapter writes, so the adapter is what asserts they agree — the
/// domain must not read a dialect to know its own values.
#[tokio::test]
async fn the_check_constraints_accept_exactly_the_enum_values() {
    let (_tmp, pool) = pool().await;
    run_all(&pool).await.unwrap();

    for kind in RepoKind::ALL {
        let name = format!("kind-{}", kind.as_str());
        insert_repository(&pool, &name, kind.as_str(), "npm")
            .await
            .unwrap();
    }
    for format in Format::ALL {
        let name = format!("format-{}", format.as_str());
        insert_repository(&pool, &name, "hosted", format.as_str())
            .await
            .unwrap();
    }
    assert!(insert_repository(&pool, "x", "mirror", "npm").await.is_err());
    assert!(insert_repository(&pool, "y", "hosted", "deb").await.is_err());

    // Agreement is mutual: inserts alone would let a CHECK admit a value the
    // enum does not model, so the DDL's own lists are compared as sets.
    let ddl: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'repositories'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        check_values(&ddl, "repo_type"),
        set(RepoKind::ALL.iter().map(|k| k.as_str()))
    );
    assert_eq!(
        check_values(&ddl, "format"),
        set(Format::ALL.iter().map(|f| f.as_str()))
    );
}

/// The quoted values of `CHECK(<column> IN ('a', 'b'))`. Reading the dialect
/// here is legitimate — it is the adapter's own — and it is what the retired
/// `kinds.rs` test used to do from the domain.
fn check_values(ddl: &str, column: &str) -> BTreeSet<String> {
    let needle = format!("CHECK({column} IN (");
    let start = ddl.find(&needle).expect("no CHECK on the column") + needle.len();
    let list = &ddl[start..][..ddl[start..].find(')').expect("unterminated CHECK")];
    list.split(',')
        .map(|value| value.trim().trim_matches('\'').to_string())
        .collect()
}

fn set<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    values.into_iter().map(str::to_string).collect()
}

/// A `.sql` file added, checksummed and listed in the README but forgotten in
/// `MIGRATIONS` passes `scripts/boundary.sh` and simply never runs.
#[test]
fn every_file_in_the_directory_is_a_migration() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/adapters/sqlite/migrations");
    let mut present: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sql"))
        .map(|name| name[..3].to_string())
        .collect();
    present.sort();

    let listed: Vec<String> = MIGRATIONS.iter().map(|m| m.id.to_string()).collect();
    let files: Vec<String> = MIGRATIONS
        .iter()
        .filter(|m| matches!(m.step, Step::Sql(_)))
        .map(|m| m.id.to_string())
        .collect();
    assert!(files.iter().all(|id| present.contains(id)), "{files:?} {present:?}");
    // A Rust step may carry its DDL in a file (024), or have none (020).
    assert!(present.iter().all(|id| listed.contains(id)), "{present:?} {listed:?}");

    // Ids are keys, so the list is what `present` can be compared against only
    // if it is itself unique and ordered.
    let mut canonical = listed.clone();
    canonical.sort();
    canonical.dedup();
    assert_eq!(canonical, listed);
}

// ---------------------------------------------------------------------------
// (d) The Rust step, which owns per-connection state
// ---------------------------------------------------------------------------

/// The rebuild, statement by statement. `nuget` is not in `001`'s CHECK list,
/// so a row carrying it proves the step really replaced the table rather than
/// silently doing nothing.
const REBUILD_SQL: [&str; 4] = [
    "CREATE TABLE repositories_new (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE,
        repo_type TEXT NOT NULL CHECK(repo_type IN ('hosted', 'proxy', 'group')),
        format TEXT NOT NULL CHECK(format IN ('npm', 'cargo', 'oci', 'go', 'pypi', 'nuget')),
        visibility TEXT NOT NULL DEFAULT 'private' CHECK(visibility IN ('public', 'private')),
        upstream_url TEXT,
        config_json TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    "INSERT INTO repositories_new SELECT * FROM repositories",
    "DROP TABLE repositories",
    "ALTER TABLE repositories_new RENAME TO repositories",
];

/// The shared `repositories` CHECK rebuild, admitting `nuget`: not a file,
/// because `PRAGMA foreign_keys` is per connection and the rename has to
/// happen with enforcement off, inside one transaction, on one connection.
fn rebuild_repositories_check(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(crate::adapters::sqlite::rebuild::widen_formats(conn, "nuget"))
}

/// The same step, refusing where the orphan diff would: mid-transaction, with
/// `foreign_keys` still off on its connection.
fn refused_rebuild(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(refused(conn))
}

async fn refused(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    begin_rebuild(conn).await?;
    rebuild_table(conn).await?;
    Err(StoreError::Other(
        "the rebuild orphaned rows and rolled back".into(),
    ))
}

async fn rebuild_table(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    for statement in REBUILD_SQL {
        exec(conn, statement).await?;
    }
    Ok(())
}

async fn begin_rebuild(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    exec(conn, "PRAGMA foreign_keys = OFF").await?;
    exec(conn, "BEGIN IMMEDIATE").await
}

async fn exec(conn: &mut SqliteConnection, sql: &str) -> Result<(), StoreError> {
    sqlx::query(sql)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(boxed)
}

fn boxed(err: sqlx::Error) -> StoreError {
    StoreError::Other(Box::new(err))
}

/// Every migration this binary carries, plus one `Rust` step at the next id
/// `nuget.md` is allocated.
fn with_step(step: fn(&mut SqliteConnection) -> StepFuture<'_>) -> Vec<Migration> {
    let mut steps: Vec<Migration> = MIGRATIONS.iter().filter(|m| m.id != "020").copied().collect();
    steps.push(Migration {
        id: "020",
        sentinel: Sentinel::Unprovable,
        step: Step::Rust(step),
    });
    steps
}

/// Every pooled connection, held at once so the pool cannot hand the same one
/// back twice: a connection a Rust step left with `foreign_keys` off would
/// silently skip the schema's cascades for one request in five.
async fn foreign_keys_on_every_connection(pool: &SqlitePool) -> Vec<i64> {
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(pool.acquire().await.unwrap());
    }
    let mut pragmas = Vec::new();
    for conn in held.iter_mut() {
        pragmas.push(
            sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                .fetch_one(&mut **conn)
                .await
                .unwrap(),
        );
    }
    pragmas
}

#[tokio::test]
async fn a_rust_step_never_returns_its_connection_to_the_pool() {
    let (_tmp, pool) = pool().await;
    run(&pool, &with_step(rebuild_repositories_check))
        .await
        .unwrap();

    insert_repository(&pool, "n", "hosted", "nuget")
        .await
        .unwrap();
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
}

#[tokio::test]
async fn a_failed_rust_step_leaves_no_connection_with_foreign_keys_off() {
    let (_tmp, pool) = pool().await;
    let err = run(&pool, &with_step(refused_rebuild)).await.unwrap_err();

    assert!(matches!(err, StoreError::Other(_)), "{err}");
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
    // The refusal rolled back with the connection it poisoned: the CHECK the
    // rebuild would have widened is still the one `001` wrote.
    assert!(insert_repository(&pool, "n", "hosted", "nuget")
        .await
        .is_err());
}

/// 025 alone on a database that predates it: every repository gets one
/// incarnation and its prefixes, and applying the file twice changes nothing.
#[tokio::test]
async fn migration_025_allocates_incarnations_and_legacy_prefixes_once() {
    let (_tmp, pool) = pool().await;
    legacy_migrate(&pool).await;
    insert_repository(&pool, "npm-hosted", "hosted", "npm").await.unwrap();
    insert_repository(&pool, "oci-proxy", "proxy", "oci").await.unwrap();
    let file = MIGRATIONS.iter().find(|m| m.id == "025").unwrap();
    let Step::Sql(sql) = file.step else {
        unreachable!("025 is a file")
    };
    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    let incarnations: Vec<String> =
        sqlx::query_scalar("SELECT incarnation FROM repository_incarnations ORDER BY repository_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(incarnations.len(), 2);
    assert_ne!(incarnations[0], incarnations[1]);
    let prefixes: Vec<String> =
        sqlx::query_scalar("SELECT prefix FROM storage_prefixes WHERE legacy = 1 ORDER BY prefix")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        prefixes,
        ["_proxy_cache/npm-hosted", "_proxy_cache/oci-proxy", "npm/npm-hosted", "oci/oci-proxy"]
    );

    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    let again: Vec<String> =
        sqlx::query_scalar("SELECT incarnation FROM repository_incarnations ORDER BY repository_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(again, incarnations, "a second apply is a no-op");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM storage_prefixes").await, 6);

    let ran = run_all(&pool).await.unwrap();
    assert_eq!(outcomes(&ran, Outcome::Adopted).last(), Some(&"025"), "its sentinel is there");
}

fn file_of(id: &str) -> &'static str {
    let Step::Sql(sql) = MIGRATIONS.iter().find(|m| m.id == id).unwrap().step else {
        unreachable!("{id} is a file")
    };
    sql
}

async fn seed_legacy_oci(pool: &SqlitePool) {
    insert_repository(pool, "oci-hosted", "hosted", "oci").await.unwrap();
    for sql in [
        "INSERT INTO oci_blobs (repository_id, digest, size) VALUES (1, 'sha256:bb', 3)",
        "INSERT INTO oci_manifests (repository_id, name, digest, content_type, size)
         VALUES (1, 'team/app', 'sha256:aa', 'application/json', 2)",
        "INSERT INTO oci_uploads (id, repository_id, name) VALUES ('u1', 1, 'team/app')",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
}

async fn assert_backfilled(pool: &SqlitePool) {
    let blob: String = sqlx::query_scalar("SELECT storage_key FROM oci_blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(blob, "oci/oci-hosted/_blobs/sha256/bb", "the key reads derived today");
    let manifest: String = sqlx::query_scalar("SELECT storage_key FROM oci_manifests")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(manifest, "oci/oci-hosted/team/app/manifests/team/app/sha256/aa");
    let legacy_uploads = "SELECT COUNT(*) FROM oci_uploads WHERE received IS NULL AND segment_prefix IS NULL";
    assert_eq!(count(pool, legacy_uploads).await, 1, "an upload started before 018 stays legacy");
    let legacy: Vec<String> =
        sqlx::query_scalar("SELECT prefix FROM storage_prefixes WHERE legacy = 1 ORDER BY prefix")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(legacy, ["_proxy_cache/oci-hosted", "oci/oci-hosted"]);
}

/// 018 on an adopted 014 database, alone and on either side of 025: every OCI
/// row gains the key it is read from today, and nothing moves.
#[tokio::test]
async fn migration_018_backfills_physical_keys_and_legacy_prefixes() {
    for first in ["018", "025"] {
        let (_tmp, pool) = pool().await;
        legacy_migrate(&pool).await;
        seed_legacy_oci(&pool).await;
        sqlx::raw_sql(file_of(first)).execute(&pool).await.unwrap();
        let ran = run_all(&pool).await.unwrap();
        assert!(outcomes(&ran, Outcome::Adopted).contains(&first), "{first} is adopted");
        assert_backfilled(&pool).await;
        assert!(run_all(&pool).await.unwrap().is_empty());
    }
}

/// A fresh database and a 014 one end with the same upload progress columns.
#[tokio::test]
async fn oci_upload_progress_applies_on_fresh_and_legacy() {
    let (_fresh_tmp, fresh) = pool().await;
    run_all(&fresh).await.unwrap();
    let (_legacy_tmp, legacy) = pool().await;
    legacy_migrate(&legacy).await;
    run_all(&legacy).await.unwrap();
    let columns = "SELECT group_concat(name) FROM pragma_table_info('oci_uploads')";
    let got: String = sqlx::query_scalar(columns).fetch_one(&fresh).await.unwrap();
    let want: String = sqlx::query_scalar(columns).fetch_one(&legacy).await.unwrap();
    assert_eq!(got, want);
    assert!(got.contains("lease_token") && got.contains("received"));
    assert_eq!(count(&fresh, "SELECT COUNT(*) FROM oci_upload_segments").await, 0);
}

async fn apply_in(order: &[&str]) -> (TempDir, SqlitePool) {
    let (tmp, pool) = pool().await;
    legacy_migrate(&pool).await;
    insert_repository(&pool, "py", "hosted", "pypi").await.unwrap();
    for id in order {
        sqlx::raw_sql(file_of(id)).execute(&pool).await.unwrap();
    }
    (tmp, pool)
}

/// 019 alone on a database that predates it, twice, and on either side of
/// 025: the same objects whichever shipped first (A1 C2).
#[tokio::test]
async fn migration_019_is_order_independent_of_025_and_idempotent() {
    let (_alone_tmp, alone) = apply_in(&["019", "019"]).await;
    assert!(objects(&alone).await.contains(&"table pypi_files".to_string()));
    let ran = run_all(&alone).await.unwrap();
    assert!(outcomes(&ran, Outcome::Adopted).contains(&"019"), "its sentinel is there");

    let (_a_tmp, before) = apply_in(&["025", "019"]).await;
    let (_b_tmp, after) = apply_in(&["019", "025"]).await;
    assert_eq!(objects(&before).await, objects(&after).await);
    let prefixes = "SELECT COUNT(*) FROM storage_prefixes WHERE prefix = 'pypi/py'";
    assert_eq!(count(&before, prefixes).await, 1);
    assert_eq!(count(&after, prefixes).await, 1);
}

/// Every migration but `id`, in order.
fn without(id: &str) -> Vec<Migration> {
    MIGRATIONS.iter().filter(|m| m.id != id).copied().collect()
}

fn only(id: &str) -> Vec<Migration> {
    MIGRATIONS.iter().filter(|m| m.id == id).copied().collect()
}

async fn repositories_ddl(pool: &SqlitePool) -> String {
    sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'repositories'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn admits(pool: &SqlitePool) -> BTreeSet<String> {
    crate::adapters::sqlite::rebuild::admitted(&repositories_ddl(pool).await).unwrap()
}

async fn rows(pool: &SqlitePool) -> Vec<(i64, String, String)> {
    sqlx::query_as("SELECT id, name, format FROM repositories ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn incarnations(pool: &SqlitePool) -> Vec<(i64, String)> {
    sqlx::query_as("SELECT repository_id, incarnation FROM repository_incarnations ORDER BY 1")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn widen_nuget(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(crate::adapters::sqlite::rebuild::widen_formats(conn, "nuget"))
}

fn widen_mcp(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(crate::adapters::sqlite::rebuild::widen_formats(conn, "mcp"))
}

/// 024 after 025, on a database holding repositories, a deleted id and a
/// row orphaned before the run: every row, id and incarnation survives with
/// the sequence, the set is the old one plus `maven`, and a second run is a
/// no-op.
#[tokio::test]
async fn migration_024_widens_the_formats_and_keeps_every_row() {
    let (_tmp, pool) = pool().await;
    run(&pool, &without("024")).await.unwrap();
    for (name, kind, format) in [("a", "hosted", "npm"), ("gone", "hosted", "go"), ("c", "proxy", "cargo")] {
        insert_repository(&pool, name, kind, format).await.unwrap();
    }
    sqlx::query("DELETE FROM repositories WHERE name = 'gone'")
        .execute(&pool)
        .await
        .unwrap();
    {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("INSERT INTO packages (repository_id, name) VALUES (999, 'orphan')")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO repository_incarnations (repository_id, incarnation)
         SELECT id, 'inc-' || name FROM repositories",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before_rows = rows(&pool).await;
    let before_incarnations = incarnations(&pool).await;
    let before_formats = admits(&pool).await;
    assert!(!before_formats.contains("maven"));

    let ran = run_all(&pool).await.unwrap();
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["024"]);

    let mut want = before_formats.clone();
    want.insert("maven".to_string());
    assert_eq!(admits(&pool).await, want);
    assert_eq!(rows(&pool).await, before_rows);
    assert_eq!(incarnations(&pool).await, before_incarnations);
    insert_repository(&pool, "m", "hosted", "maven").await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM repositories WHERE name = 'm'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(id, 4, "the sequence survived the rebuild: id 2 is never handed out again");
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM pragma_foreign_key_check").await,
        1,
        "only the orphan that predates the run"
    );
    assert!(run_all(&pool).await.unwrap().is_empty());
    for table in ["maven_values", "maven_units", "maven_files", "maven_declarations"] {
        let sql = format!("SELECT COUNT(*) FROM sqlite_master WHERE name = '{table}'");
        assert_eq!(count(&pool, &sql).await, 1, "{table}");
    }
}

/// A repository already named `maven` would be shadowed by the mount: the
/// step refuses with a message naming it, and changes nothing.
#[tokio::test]
async fn migration_024_refuses_a_repository_named_maven_and_changes_nothing() {
    let (_tmp, pool) = pool().await;
    run(&pool, &without("024")).await.unwrap();
    insert_repository(&pool, "maven", "hosted", "npm").await.unwrap();
    let ddl = repositories_ddl(&pool).await;

    let err = run_all(&pool).await.unwrap_err().to_string();
    assert!(err.contains("'maven'") && err.contains("/maven/"), "{err}");
    assert_eq!(repositories_ddl(&pool).await, ddl);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'maven%'").await,
        0
    );
    assert!(!applied(&pool).await.unwrap().iter().any(|id| id == "024"));
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
}

/// 024 alone on a database that predates 018 and 025; 025 after it still
/// gives every repository an incarnation.
#[tokio::test]
async fn migration_024_runs_alone_on_a_legacy_database_and_before_025() {
    let (_tmp, pool) = pool().await;
    let pre_018: Vec<Migration> = MIGRATIONS.iter().filter(|m| m.id < "018").copied().collect();
    run(&pool, &pre_018).await.unwrap();
    insert_repository(&pool, "npm-hosted", "hosted", "npm").await.unwrap();

    let ran = run(&pool, &only("024")).await.unwrap();
    assert_eq!(ran, vec![("024", Outcome::Applied)]);
    assert!(admits(&pool).await.contains("maven"));
    insert_repository(&pool, "mvn", "hosted", "maven").await.unwrap();

    let ran = run_all(&pool).await.unwrap();
    assert!(ran.contains(&("025", Outcome::Applied)), "{ran:?}");
    assert_eq!(incarnations(&pool).await.len(), 2);
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
}

/// Whatever order the format migrations land in, the table ends admitting
/// the union and keeps its incarnations; each applied twice is a no-op.
#[tokio::test]
async fn format_widenings_end_with_the_union_in_every_order() {
    type StepFn = fn(&mut SqliteConnection) -> StepFuture<'_>;
    let steps: [(&'static str, StepFn); 3] =
        [("020", widen_nuget), ("023", widen_mcp), ("024", maven)];
    let orders = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    let mut want: BTreeSet<String> = Format::ALL.iter().map(|f| f.as_str().to_string()).collect();
    want.insert("nuget".to_string());
    want.insert("mcp".to_string());
    for order in orders {
        let (_tmp, pool) = pool().await;
        run(&pool, &without("024")).await.unwrap();
        insert_repository(&pool, "keep", "hosted", "npm").await.unwrap();
        let kept = incarnations(&pool).await;
        let list: Vec<Migration> = order
            .iter()
            .map(|&i| Migration {
                id: steps[i].0,
                sentinel: Sentinel::Unprovable,
                step: Step::Rust(steps[i].1),
            })
            .collect();
        run(&pool, &list).await.unwrap();
        for m in &list {
            apply(&pool, &m.step).await.unwrap();
        }
        assert_eq!(admits(&pool).await, want, "{order:?}");
        assert_eq!(incarnations(&pool).await, kept);
        assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
    }
}

// ---------------------------------------------------------------------------
// (e) A1 C2: the shared format-widening rebuild, 020 its first user
// ---------------------------------------------------------------------------

fn widen_maven(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(crate::adapters::sqlite::rebuild::widen_formats(conn, "maven"))
}

/// 023 and 024 stand in for mcp and Maven, the helper's other users.
fn step(id: &'static str) -> Migration {
    match id {
        "023" => Migration {
            id,
            sentinel: Sentinel::Unprovable,
            step: Step::Rust(widen_mcp),
        },
        "024" => Migration {
            id,
            sentinel: Sentinel::Unprovable,
            step: Step::Rust(widen_maven),
        },
        _ => *MIGRATIONS.iter().find(|m| m.id == id).unwrap(),
    }
}

async fn admitted(pool: &SqlitePool) -> BTreeSet<String> {
    let ddl: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'repositories'")
        .fetch_one(pool)
        .await
        .unwrap();
    crate::adapters::sqlite::rebuild::admitted(&ddl).unwrap()
}

async fn incarnation_rows(pool: &SqlitePool) -> Vec<(i64, String)> {
    sqlx::query_as("SELECT repository_id, incarnation FROM repository_incarnations ORDER BY 1")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The legacy files, two repositories and a package, a third repository
/// deleted so the id sequence is ahead of the highest surviving id.
async fn seeded_pre_018() -> (TempDir, SqlitePool) {
    let (tmp, pool) = pool().await;
    legacy_migrate(&pool).await;
    insert_repository(&pool, "npm-hosted", "hosted", "npm").await.unwrap();
    insert_repository(&pool, "gone", "hosted", "cargo").await.unwrap();
    insert_repository(&pool, "go-proxy", "proxy", "go").await.unwrap();
    sqlx::raw_sql(
        "DELETE FROM repositories WHERE name = 'go-proxy';
         INSERT INTO packages (repository_id, name) VALUES (1, 'left-pad');",
    )
    .execute(&pool)
    .await
    .unwrap();
    (tmp, pool)
}

async fn run_steps(pool: &SqlitePool, ids: &[&'static str]) {
    let steps: Vec<Migration> = ids.iter().map(|id| step(id)).collect();
    run(pool, &steps).await.unwrap();
}

#[tokio::test]
async fn migration_020_alone_admits_nuget_and_keeps_every_row() {
    let (_tmp, pool) = seeded_pre_018().await;
    run_steps(&pool, &["020"]).await;

    assert_eq!(
        admitted(&pool).await,
        set(["npm", "cargo", "oci", "go", "pypi", "nuget"])
    );
    insert_repository(&pool, "nuget-hosted", "hosted", "nuget").await.unwrap();
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ids, [1, 2, 4], "a deleted repository's id is never handed out again");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM packages").await, 1);
    assert!(insert_repository(&pool, "deb", "hosted", "deb").await.is_err());
    assert_eq!(foreign_keys_on_every_connection(&pool).await, vec![1; 5]);
}

#[tokio::test]
async fn migration_020_twice_is_a_no_op() {
    let (_tmp, pool) = seeded_pre_018().await;
    run_steps(&pool, &["020"]).await;
    let before: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'repositories'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    crate::adapters::sqlite::rebuild::widen_formats(&mut conn, "nuget").await.unwrap();
    drop(conn);
    let after: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'repositories'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(after.matches("'nuget'").count(), 1);
}

#[tokio::test]
async fn migration_020_after_023_keeps_what_023_added() {
    let (_tmp, pool) = seeded_pre_018().await;
    run_steps(&pool, &["023", "020"]).await;
    let formats = admitted(&pool).await;
    assert!(formats.contains("mcp") && formats.contains("nuget"), "{formats:?}");
}

#[tokio::test]
async fn every_order_of_the_helper_users_ends_with_the_union() {
    let orders: [[&'static str; 3]; 6] = [
        ["020", "023", "024"],
        ["020", "024", "023"],
        ["023", "020", "024"],
        ["023", "024", "020"],
        ["024", "020", "023"],
        ["024", "023", "020"],
    ];
    let expected = set(["npm", "cargo", "oci", "go", "pypi", "nuget", "mcp", "maven"]);
    for order in orders {
        let (_tmp, pool) = seeded_pre_018().await;
        run_steps(&pool, &order).await;
        assert_eq!(admitted(&pool).await, expected, "{order:?}");
    }
}

#[tokio::test]
async fn migration_025_before_or_after_020_keeps_incarnations() {
    for order in [["025", "020"], ["020", "025"]] {
        let (_tmp, pool) = seeded_pre_018().await;
        run_steps(&pool, &order).await;
        let before = incarnation_rows(&pool).await;
        assert_eq!(before.len(), 2, "{order:?}");
        run_steps(&pool, &["023"]).await;
        assert_eq!(incarnation_rows(&pool).await, before, "{order:?}");
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM pragma_foreign_key_check").await, 0);
        sqlx::query("DELETE FROM repositories WHERE name = 'gone'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            incarnation_rows(&pool).await.len(),
            1,
            "the cascade of 025 survives the rebuild: {order:?}"
        );
    }
}

/// The `server_secrets` half of ha-options' 022, as A1 C2 fixes it: the same
/// definition, created only if 021 did not.
const SECRETS_022: &str = "CREATE TABLE IF NOT EXISTS server_secrets (
    name TEXT PRIMARY KEY,
    value BLOB NOT NULL,
    created_at TEXT NOT NULL
);";

async fn secrets_definition(pool: &SqlitePool) -> String {
    sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'server_secrets'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// 021 alone on a pre-018 database, twice: the SSO tables appear once and a
/// second apply changes nothing.
#[tokio::test]
async fn migration_021_alone_on_a_pre_018_database_is_idempotent() {
    let (_tmp, pool) = pool().await;
    legacy_migrate(&pool).await;
    sqlx::raw_sql(file_of("021")).execute(&pool).await.unwrap();
    let before = objects(&pool).await;
    sqlx::raw_sql(file_of("021")).execute(&pool).await.unwrap();
    assert_eq!(objects(&pool).await, before);
    for table in ["server_secrets", "sso_identities", "login_handoffs", "sso_token_provenance"] {
        assert!(before.contains(&format!("table {table}")), "{table}");
    }
}

/// `server_secrets` is created by whichever of 021 and 022 ships first; the
/// other checks and skips, in both orders, and ends on one definition.
#[tokio::test]
async fn migrations_021_and_022_create_server_secrets_in_either_order() {
    let (_a, first) = pool().await;
    legacy_migrate(&first).await;
    sqlx::raw_sql(file_of("021")).execute(&first).await.unwrap();
    sqlx::raw_sql(SECRETS_022).execute(&first).await.unwrap();

    let (_b, second) = pool().await;
    legacy_migrate(&second).await;
    sqlx::raw_sql(SECRETS_022).execute(&second).await.unwrap();
    sqlx::raw_sql(file_of("021")).execute(&second).await.unwrap();

    assert_eq!(objects(&first).await, objects(&second).await);
    let normalize = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        normalize(secrets_definition(&first).await),
        normalize(secrets_definition(&second).await)
    );
}

/// Every shipped format migration together: a fresh database admits each
/// format the domain knows, and nothing else.
#[tokio::test]
async fn a_fully_migrated_database_admits_every_format_together() {
    let (_tmp, pool) = pool().await;
    run_all(&pool).await.unwrap();
    for format in Format::ALL {
        insert_repository(&pool, &format!("r-{}", format.as_str()), "hosted", format.as_str())
            .await
            .unwrap();
    }
    assert_eq!(
        admits(&pool).await,
        set(["npm", "cargo", "oci", "go", "pypi", "maven", "nuget"])
    );
    assert!(insert_repository(&pool, "deb", "hosted", "deb").await.is_err());
}

/// 026 on a database that predates it: a token written before scopes reads
/// back as `inherit`, and a grant that could write can still delete now that
/// the routes ask for the verb.
#[tokio::test]
async fn migration_026_makes_every_token_inherit_and_keeps_the_delete_rung() {
    let (_tmp, pool) = pool().await;
    legacy_migrate(&pool).await;
    insert_repository(&pool, "npm-hosted", "hosted", "npm").await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO users (username, password_hash, role) VALUES ('ci', 'h', 'publisher');
         INSERT INTO api_tokens (id, user_id, name, prefix, token_hash)
             VALUES ('t1', 1, 'robot', 'trg_000000000000', 'hash');
         INSERT INTO user_permissions (user_id, repository_id, can_read, can_write, can_delete)
             VALUES (1, 1, 1, 1, 0);",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(file_of("026")).execute(&pool).await.unwrap();

    let scope: String = sqlx::query_scalar("SELECT scope FROM api_tokens WHERE id = 't1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(crate::domain::TokenScope::parse(&scope).unwrap().is_inherit(), "{scope}");
    assert_eq!(
        count(&pool, "SELECT can_delete FROM user_permissions WHERE user_id = 1").await,
        1,
        "a grant that could write keeps removing"
    );

    // A binary that predates scopes writes no column, and the row it creates is
    // still readable: that is the whole of the two-binary window.
    sqlx::raw_sql(
        "INSERT INTO api_tokens (id, user_id, name, prefix, token_hash)
             VALUES ('t2', 1, 'login', 'trg_111111111111', 'hash2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let old_binary: String = sqlx::query_scalar("SELECT scope FROM api_tokens WHERE id = 't2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(crate::domain::TokenScope::parse(&old_binary).unwrap().is_inherit());

    let ran = run_all(&pool).await.unwrap();
    assert!(outcomes(&ran, Outcome::Adopted).contains(&"026"), "its sentinel is there");
}
