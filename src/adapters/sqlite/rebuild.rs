//! The `repositories` CHECK rebuild every format migration shares (A1 C2).
//!
//! SQLite cannot alter a CHECK, so admitting a format is the twelve-step
//! table rebuild: foreign keys off on this one connection, one transaction,
//! the rows copied, the indexes and triggers recreated, the AUTOINCREMENT
//! sequence restored, and an orphan diff that rolls everything back when the
//! rebuild would leave a row pointing nowhere. The allowed set is always what
//! the table admits now plus one format, never a literal list, so migrations
//! that widen it compose in any order.

use std::collections::BTreeSet;

use sqlx::SqliteConnection;

use crate::error::StoreError;

const TABLE: &str = "repositories";
const STAGING: &str = "repositories_widened";

fn other(err: sqlx::Error) -> StoreError {
    StoreError::Other(Box::new(err))
}

fn refused(why: String) -> StoreError {
    StoreError::Other(why.into())
}

async fn exec(conn: &mut SqliteConnection, sql: &str) -> Result<(), StoreError> {
    sqlx::query(sql)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(other)
}

async fn table_sql(conn: &mut SqliteConnection) -> Result<String, StoreError> {
    sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .bind(TABLE)
        .fetch_optional(&mut *conn)
        .await
        .map_err(other)?
        .ok_or_else(|| refused(format!("no {TABLE} table to widen")))
}

/// Where the quoted list of `CHECK(format IN (...))` starts and ends.
fn format_list(ddl: &str) -> Result<(usize, usize), StoreError> {
    let needle = "CHECK(format IN (";
    let start = ddl
        .find(needle)
        .ok_or_else(|| refused(format!("{TABLE} has no format CHECK")))?
        + needle.len();
    let end = start
        + ddl[start..]
            .find(')')
            .ok_or_else(|| refused(format!("{TABLE} has an unterminated format CHECK")))?;
    Ok((start, end))
}

/// The formats `ddl` admits.
pub(crate) fn admitted(ddl: &str) -> Result<BTreeSet<String>, StoreError> {
    let (start, end) = format_list(ddl)?;
    Ok(ddl[start..end]
        .split(',')
        .map(|v| v.trim().trim_matches('\'').to_string())
        .filter(|v| !v.is_empty())
        .collect())
}

/// `ddl` admitting `format` too, as the staging table.
fn widened(ddl: &str, format: &str) -> Result<String, StoreError> {
    let (start, end) = format_list(ddl)?;
    let body = ddl
        .find('(')
        .ok_or_else(|| refused(format!("unreadable {TABLE} definition")))?;
    let list = format!("{}, '{format}'", &ddl[start..end]);
    Ok(format!(
        "CREATE TABLE {STAGING} {}{list}{}",
        &ddl[body..start],
        &ddl[end..]
    ))
}

async fn orphans(conn: &mut SqliteConnection) -> Result<BTreeSet<String>, StoreError> {
    let rows: Vec<(String, Option<i64>, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await
        .map_err(other)?;
    Ok(rows
        .into_iter()
        .map(|(table, rowid, parent, fk)| format!("{table}:{rowid:?}:{parent}:{fk}"))
        .collect())
}

/// Make `repositories.format` admit `format`; a no-op when it already does.
pub(crate) async fn widen_formats(
    conn: &mut SqliteConnection,
    format: &str,
) -> Result<(), StoreError> {
    let ddl = table_sql(conn).await?;
    if admitted(&ddl)?.contains(format) {
        return Ok(());
    }
    let staging = widened(&ddl, format)?;
    exec(conn, "PRAGMA foreign_keys = OFF").await?;
    exec(conn, "BEGIN IMMEDIATE").await?;
    match rebuild(conn, &staging).await {
        Ok(()) => {
            exec(conn, "COMMIT").await?;
            exec(conn, "PRAGMA foreign_keys = ON").await
        }
        Err(err) => {
            let _ = exec(conn, "ROLLBACK").await;
            let _ = exec(conn, "PRAGMA foreign_keys = ON").await;
            Err(err)
        }
    }
}

async fn rebuild(conn: &mut SqliteConnection, staging: &str) -> Result<(), StoreError> {
    let before = orphans(conn).await?;
    let dependents: Vec<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master
         WHERE tbl_name = ?1 AND type IN ('index', 'trigger') AND sql IS NOT NULL
         ORDER BY type, name",
    )
    .bind(TABLE)
    .fetch_all(&mut *conn)
    .await
    .map_err(other)?;
    let seq: Option<i64> = sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = ?1")
        .bind(TABLE)
        .fetch_optional(&mut *conn)
        .await
        .map_err(other)?;

    exec(conn, staging).await?;
    exec(conn, &format!("INSERT INTO {STAGING} SELECT * FROM {TABLE}")).await?;
    exec(conn, &format!("DROP TABLE {TABLE}")).await?;
    exec(conn, &format!("ALTER TABLE {STAGING} RENAME TO {TABLE}")).await?;
    for sql in &dependents {
        exec(conn, sql).await?;
    }
    if let Some(seq) = seq {
        sqlx::query("UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = ?2")
            .bind(seq)
            .bind(TABLE)
            .execute(&mut *conn)
            .await
            .map_err(other)?;
    }
    let after = orphans(conn).await?;
    if let Some(new) = after.difference(&before).next() {
        return Err(refused(format!(
            "widening {TABLE} would orphan a row ({new}); nothing was changed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDL: &str = "CREATE TABLE \"repositories\" (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    format TEXT NOT NULL CHECK(format IN ('npm', 'cargo')),\n    name TEXT\n)";

    #[test]
    fn the_widened_definition_keeps_every_column_and_adds_one_value() {
        let staging = widened(DDL, "maven").unwrap();
        assert!(staging.starts_with("CREATE TABLE repositories_widened ("));
        assert!(staging.contains("CHECK(format IN ('npm', 'cargo', 'maven'))"));
        assert!(staging.ends_with("name TEXT\n)"));
        assert_eq!(
            admitted(&staging).unwrap(),
            ["cargo", "maven", "npm"].iter().map(|s| s.to_string()).collect()
        );
    }
}
