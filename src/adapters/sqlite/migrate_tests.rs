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
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["015", "017", "021", "025"]);

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
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["013", "014", "015", "017", "021", "025"]);

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
    assert_eq!(outcomes(&ran, Outcome::Applied), vec!["015", "017", "021", "025"]);
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
    assert_eq!(present, listed);

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

/// The `repositories` CHECK rebuild `nuget.md` and `maven.md` both need: not a
/// file, because `PRAGMA foreign_keys` is per connection and the rename has to
/// happen with enforcement off, inside one transaction, on one connection.
fn rebuild_repositories_check(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(rebuild(conn))
}

/// The same step, refusing where the orphan diff would: mid-transaction, with
/// `foreign_keys` still off on its connection.
fn refused_rebuild(conn: &mut SqliteConnection) -> StepFuture<'_> {
    Box::pin(refused(conn))
}

async fn rebuild(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    begin_rebuild(conn).await?;
    let old_seq: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'repositories'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(boxed)?;
    rebuild_table(conn).await?;
    if let Some(seq) = old_seq {
        // A rebuilt AUTOINCREMENT table restarts at its highest surviving id,
        // so a deleted repository's id could otherwise be handed out again.
        sqlx::query("UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'repositories'")
            .bind(seq)
            .execute(&mut *conn)
            .await
            .map_err(boxed)?;
    }
    exec(conn, "COMMIT").await?;
    exec(conn, "PRAGMA foreign_keys = ON").await
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
    let mut steps = MIGRATIONS.to_vec();
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

fn sql_of(id: &str) -> &'static str {
    let Step::Sql(sql) = MIGRATIONS.iter().find(|m| m.id == id).unwrap().step else {
        unreachable!("{id} is a file")
    };
    sql
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
    sqlx::raw_sql(sql_of("021")).execute(&pool).await.unwrap();
    let before = objects(&pool).await;
    sqlx::raw_sql(sql_of("021")).execute(&pool).await.unwrap();
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
    sqlx::raw_sql(sql_of("021")).execute(&first).await.unwrap();
    sqlx::raw_sql(SECRETS_022).execute(&first).await.unwrap();

    let (_b, second) = pool().await;
    legacy_migrate(&second).await;
    sqlx::raw_sql(SECRETS_022).execute(&second).await.unwrap();
    sqlx::raw_sql(sql_of("021")).execute(&second).await.unwrap();

    assert_eq!(objects(&first).await, objects(&second).await);
    let normalize = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        normalize(secrets_definition(&first).await),
        normalize(secrets_definition(&second).await)
    );
}
