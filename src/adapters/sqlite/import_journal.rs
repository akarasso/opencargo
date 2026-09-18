//! The importer's resume journal: one SQLite file of its own, never the
//! registry's database, and never a secret.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::domain::import::{GapKind, ItemStatus};
use crate::ports::import::{
    needs_redaction, Cursor, Gap, ImportJournal, Journaled, JournalError, Lane, Outcome, PkgExtra,
    Planned, RunHeader, Unsealed,
};

use super::{bind_ts, parse_ts};

const USER_VERSION: i64 = 1;
const STALE_OWNER_SECS: i64 = 60;
const RUN_SCOPE: &str = "run";
const COLLISION_SCOPE: &str = "collision";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS run (id INTEGER PRIMARY KEY CHECK (id = 1), source TEXT NOT NULL,
  source_url TEXT NOT NULL, target_url TEXT NOT NULL, opts_json TEXT NOT NULL, started_at TEXT NOT NULL,
  finished_at TEXT, phase TEXT NOT NULL, owner TEXT, heartbeat TEXT);
CREATE TABLE IF NOT EXISTS cursor (stream TEXT PRIMARY KEY, token TEXT, done INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS item (source_ref TEXT PRIMARY KEY, stream TEXT NOT NULL,
  target_repo TEXT NOT NULL, target_name TEXT NOT NULL, target_format TEXT NOT NULL, version TEXT NOT NULL,
  planned_json TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
  claimed_at TEXT, bytes INTEGER, sha256 TEXT, error TEXT, note TEXT);
CREATE INDEX IF NOT EXISTS item_ready ON item(status, target_repo, target_name, version);
CREATE TABLE IF NOT EXISTS pkg (target_repo TEXT NOT NULL, name TEXT NOT NULL, format TEXT NOT NULL,
  extra_json TEXT NOT NULL, sealed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (target_repo, name));
CREATE TABLE IF NOT EXISTS gap (scope TEXT NOT NULL, kind TEXT NOT NULL, source_ref TEXT NOT NULL,
  detail TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 1, PRIMARY KEY (scope, kind, source_ref, detail));
";

pub struct SqliteImportJournal {
    pool: SqlitePool,
    path: PathBuf,
}

fn io(e: impl std::fmt::Display) -> JournalError {
    JournalError::Io(e.to_string())
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<(), JournalError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if !dir.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(io)?;
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<(), JournalError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(path).map_err(io)?;
    Ok(())
}

impl SqliteImportJournal {
    /// Opens or creates the file; `create: false` refuses a missing one.
    pub async fn open(path: &Path, create: bool) -> Result<Self, JournalError> {
        if !path.exists() {
            if !create {
                return Err(JournalError::Io(format!("{} does not exist", path.display())));
            }
            create_private(path)?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(io)?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(io)?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .map_err(io)?;
        let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table'")
            .fetch_one(&pool)
            .await
            .map_err(io)?;
        if version != USER_VERSION && !(version == 0 && tables == 0) {
            return Err(JournalError::Incompatible(format!(
                "{} was written by another version of the importer (user_version {version}); delete it with `opencargo import forget` and start again",
                path.display()
            )));
        }
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(io)?;
        sqlx::raw_sql(&format!("PRAGMA user_version = {USER_VERSION}"))
            .execute(&pool)
            .await
            .map_err(io)?;
        Ok(Self { pool, path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, JournalError> {
    serde_json::to_string(v).map_err(io)
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, JournalError> {
    serde_json::from_str(s).map_err(|e| JournalError::Incompatible(format!("unreadable row: {e}")))
}

fn check_redacted(p: &Planned) -> Result<(), JournalError> {
    for s in p.item.origin.strings() {
        if needs_redaction(s) {
            return Err(JournalError::Unredacted(format!(
                "{}: an origin may hold coordinates only, not a URL with credentials or a query",
                p.item.source_ref
            )));
        }
    }
    Ok(())
}

fn journaled(row: &sqlx::sqlite::SqliteRow) -> Result<Journaled, JournalError> {
    let status: String = row.get("status");
    Ok(Journaled {
        planned: from_json(row.get::<&str, _>("planned_json"))?,
        status: ItemStatus::from_str(&status).map_err(|e| JournalError::Incompatible(e.to_string()))?,
        attempts: row.get::<i64, _>("attempts") as u32,
        bytes: row.get::<Option<i64>, _>("bytes").map(|b| b as u64),
        sha256: row.get("sha256"),
        error: row.get("error"),
        note: row.get("note"),
    })
}

fn gap_of(row: &sqlx::sqlite::SqliteRow) -> Result<Gap, JournalError> {
    let kind: String = row.get("kind");
    Ok(Gap {
        kind: GapKind::from_str(&kind).map_err(|e| JournalError::Incompatible(e.to_string()))?,
        source_ref: row.get("source_ref"),
        detail: row.get("detail"),
    })
}

async fn insert_gaps(
    tx: &mut sqlx::SqliteConnection,
    scope: &str,
    gaps: &[Gap],
) -> Result<(), sqlx::Error> {
    for g in gaps {
        sqlx::query(
            "INSERT INTO gap (scope, kind, source_ref, detail) VALUES (?, ?, ?, ?)
             ON CONFLICT(scope, kind, source_ref, detail) DO UPDATE SET count = count + 1",
        )
        .bind(scope)
        .bind(g.kind.as_str())
        .bind(&g.source_ref)
        .bind(&g.detail)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

#[async_trait]
impl ImportJournal for SqliteImportJournal {
    async fn header(&self) -> Result<Option<RunHeader>, JournalError> {
        let row = sqlx::query("SELECT * FROM run WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(io)?;
        let Some(row) = row else { return Ok(None) };
        let ts = |col: &str| -> Result<Option<DateTime<Utc>>, JournalError> {
            match row.get::<Option<String>, _>(col) {
                Some(s) => parse_ts(&s)
                    .map(Some)
                    .ok_or_else(|| JournalError::Incompatible(format!("unreadable {col}: {s}"))),
                None => Ok(None),
            }
        };
        Ok(Some(RunHeader {
            source: row.get("source"),
            source_url: row.get("source_url"),
            target_url: row.get("target_url"),
            opts_json: row.get("opts_json"),
            started_at: ts("started_at")?.unwrap_or_default(),
            finished_at: ts("finished_at")?,
            phase: row.get("phase"),
            owner: row.get("owner"),
        }))
    }

    async fn begin(
        &self,
        header: &RunHeader,
        owner: &str,
        fresh: bool,
        now: DateTime<Utc>,
    ) -> Result<(), JournalError> {
        for s in [&header.source_url, &header.target_url] {
            if needs_redaction(s) {
                return Err(JournalError::Unredacted(format!("{s} is not redacted")));
            }
        }
        let mut tx = self.pool.begin().await.map_err(io)?;
        let held = sqlx::query("SELECT owner, heartbeat FROM run WHERE id = 1")
            .fetch_optional(&mut *tx)
            .await
            .map_err(io)?;
        if let Some(row) = held {
            let other: Option<String> = row.get("owner");
            let beat = row.get::<Option<String>, _>("heartbeat").and_then(|s| parse_ts(&s));
            if let (Some(other), Some(beat)) = (other, beat) {
                if other != owner && (now - beat).num_seconds() < STALE_OWNER_SECS {
                    return Err(JournalError::Busy(other));
                }
            }
        }
        let started = if fresh { bind_ts(header.started_at) } else {
            sqlx::query_scalar::<_, String>("SELECT started_at FROM run WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(io)?
                .unwrap_or_else(|| bind_ts(header.started_at))
        };
        sqlx::query(
            "INSERT INTO run (id, source, source_url, target_url, opts_json, started_at, finished_at, phase, owner, heartbeat)
             VALUES (1, ?, ?, ?, ?, ?, NULL, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET source = excluded.source, source_url = excluded.source_url,
               target_url = excluded.target_url, opts_json = excluded.opts_json, started_at = excluded.started_at,
               finished_at = NULL, phase = excluded.phase, owner = excluded.owner, heartbeat = excluded.heartbeat",
        )
        .bind(&header.source)
        .bind(&header.source_url)
        .bind(&header.target_url)
        .bind(&header.opts_json)
        .bind(started)
        .bind(&header.phase)
        .bind(owner)
        .bind(bind_ts(now))
        .execute(&mut *tx)
        .await
        .map_err(io)?;
        sqlx::query("UPDATE item SET status = 'pending' WHERE status = 'running'")
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        sqlx::query("UPDATE item SET status = 'pending', attempts = 0, error = NULL WHERE status = 'failed'")
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        if fresh {
            for stmt in [
                "UPDATE cursor SET token = NULL, done = 0",
                "DELETE FROM item",
                "DELETE FROM pkg",
                "DELETE FROM gap WHERE scope LIKE 'item:%' OR scope LIKE 'seal:%' OR scope = 'collision'",
            ] {
                sqlx::query(stmt).execute(&mut *tx).await.map_err(io)?;
            }
        }
        tx.commit().await.map_err(io)
    }

    async fn heartbeat(&self, owner: &str, now: DateTime<Utc>) -> Result<(), JournalError> {
        sqlx::query("UPDATE run SET heartbeat = ? WHERE id = 1 AND owner = ?")
            .bind(bind_ts(now))
            .bind(owner)
            .execute(&self.pool)
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn finish(&self, phase: &str, now: DateTime<Utc>) -> Result<(), JournalError> {
        sqlx::query("UPDATE run SET phase = ?, finished_at = ?, owner = NULL, heartbeat = NULL WHERE id = 1")
            .bind(phase)
            .bind(bind_ts(now))
            .execute(&self.pool)
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn streams(&self, names: &[String]) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        for n in names {
            sqlx::query("INSERT INTO cursor (stream) VALUES (?) ON CONFLICT(stream) DO NOTHING")
                .bind(n)
                .execute(&mut *tx)
                .await
                .map_err(io)?;
        }
        tx.commit().await.map_err(io)
    }

    async fn cursor(&self, stream: &str) -> Result<(Cursor, bool), JournalError> {
        let row = sqlx::query("SELECT token, done FROM cursor WHERE stream = ?")
            .bind(stream)
            .fetch_optional(&self.pool)
            .await
            .map_err(io)?;
        Ok(row.map_or((None, false), |r| (r.get("token"), r.get::<i64, _>("done") != 0)))
    }

    async fn record(
        &self,
        stream: &str,
        restarted: bool,
        planned: &[Planned],
        gaps: &[Gap],
        cursor: &Cursor,
        done: bool,
    ) -> Result<(), JournalError> {
        for p in planned {
            check_redacted(p)?;
        }
        let mut tx = self.pool.begin().await.map_err(io)?;
        if restarted {
            sqlx::query("DELETE FROM gap WHERE scope = ?")
                .bind(stream)
                .execute(&mut *tx)
                .await
                .map_err(io)?;
        }
        for p in planned {
            sqlx::query(
                "INSERT INTO item (source_ref, stream, target_repo, target_name, target_format, version, planned_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(source_ref) DO UPDATE SET stream = excluded.stream, target_repo = excluded.target_repo,
                   target_name = excluded.target_name, target_format = excluded.target_format,
                   version = excluded.version, planned_json = excluded.planned_json",
            )
            .bind(&p.item.source_ref)
            .bind(stream)
            .bind(&p.target_repo)
            .bind(&p.target_name)
            .bind(p.target_format.as_str())
            .bind(&p.item.coord.version)
            .bind(to_json(p)?)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
            sqlx::query(
                "INSERT INTO pkg (target_repo, name, format, extra_json) VALUES (?, ?, ?, ?)
                 ON CONFLICT(target_repo, name) DO UPDATE SET extra_json = excluded.extra_json",
            )
            .bind(&p.target_repo)
            .bind(&p.target_name)
            .bind(p.target_format.as_str())
            .bind(to_json(&p.item.pkg)?)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        }
        insert_gaps(&mut tx, stream, gaps).await.map_err(io)?;
        sqlx::query(
            "INSERT INTO cursor (stream, token, done) VALUES (?, ?, ?)
             ON CONFLICT(stream) DO UPDATE SET token = excluded.token, done = excluded.done",
        )
        .bind(stream)
        .bind(cursor)
        .bind(done as i64)
        .execute(&mut *tx)
        .await
        .map_err(io)?;
        tx.commit().await.map_err(io)
    }

    async fn replace_run_gaps(&self, gaps: &[Gap]) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        sqlx::query("DELETE FROM gap WHERE scope = ?")
            .bind(RUN_SCOPE)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        insert_gaps(&mut tx, RUN_SCOPE, gaps).await.map_err(io)?;
        tx.commit().await.map_err(io)
    }

    async fn claim(&self, lane: Lane, now: DateTime<Utc>) -> Result<Option<Journaled>, JournalError> {
        let row = sqlx::query(
            "UPDATE item SET status = 'running', attempts = attempts + 1, claimed_at = ?
             WHERE source_ref = (SELECT source_ref FROM item WHERE status = 'pending' AND (target_format = 'oci') = ?
                                 ORDER BY target_repo, target_name, version LIMIT 1)
             RETURNING *",
        )
        .bind(bind_ts(now))
        .bind(lane == Lane::Blob)
        .fetch_optional(&self.pool)
        .await
        .map_err(io)?;
        row.as_ref().map(journaled).transpose()
    }

    async fn complete(&self, source_ref: &str, o: &Outcome) -> Result<(), JournalError> {
        let scope = format!("item:{source_ref}");
        let mut tx = self.pool.begin().await.map_err(io)?;
        sqlx::query("UPDATE item SET status = ?, bytes = ?, sha256 = ?, error = ?, note = ? WHERE source_ref = ?")
            .bind(o.status.as_str())
            .bind(o.bytes.map(|b| b as i64))
            .bind(&o.sha256)
            .bind(&o.error)
            .bind(&o.note)
            .bind(source_ref)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        sqlx::query("DELETE FROM gap WHERE scope = ?").bind(&scope).execute(&mut *tx).await.map_err(io)?;
        insert_gaps(&mut tx, &scope, &o.gaps).await.map_err(io)?;
        if o.status != ItemStatus::Failed {
            sqlx::query(
                "UPDATE pkg SET sealed = 0 WHERE (target_repo, name) IN
                   (SELECT target_repo, target_name FROM item WHERE source_ref = ?)",
            )
            .bind(source_ref)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        }
        tx.commit().await.map_err(io)
    }

    async fn take_collisions(&self) -> Result<Vec<Gap>, JournalError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let rows = sqlx::query(
            "SELECT target_repo, target_name, version, group_concat(source_ref, ' and ') AS refs FROM
               (SELECT target_repo, target_name, version, source_ref FROM item ORDER BY source_ref)
             GROUP BY target_repo, target_name, version HAVING count(*) > 1
             ORDER BY target_repo, target_name, version",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(io)?;
        let mut gaps = Vec::new();
        for r in rows {
            let (repo, name, version, refs): (String, String, String, String) =
                (r.get("target_repo"), r.get("target_name"), r.get("version"), r.get("refs"));
            sqlx::query(
                "DELETE FROM item WHERE target_repo = ? AND target_name = ? AND version = ?
                   AND status IN ('pending', 'running', 'failed')",
            )
            .bind(&repo)
            .bind(&name)
            .bind(&version)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
            gaps.push(Gap::new(
                GapKind::TargetCollision,
                format!("{repo}/{name}@{version}"),
                format!("{refs} map to one target coordinate"),
            ));
        }
        insert_gaps(&mut tx, COLLISION_SCOPE, &gaps).await.map_err(io)?;
        tx.commit().await.map_err(io)?;
        Ok(gaps)
    }

    async fn targets(&self) -> Result<Vec<(String, crate::domain::Format)>, JournalError> {
        let rows = sqlx::query("SELECT DISTINCT target_repo, target_format FROM item ORDER BY target_repo, target_format")
            .fetch_all(&self.pool)
            .await
            .map_err(io)?;
        rows.iter()
            .map(|r| {
                let f: String = r.get("target_format");
                Ok((
                    r.get("target_repo"),
                    crate::domain::Format::from_str(&f).map_err(|e| JournalError::Incompatible(e.to_string()))?,
                ))
            })
            .collect()
    }

    async fn unsealed(&self) -> Result<Vec<Unsealed>, JournalError> {
        let rows = sqlx::query(
            "SELECT p.target_repo, p.name, p.format, p.extra_json FROM pkg p WHERE p.sealed = 0
               AND NOT EXISTS (SELECT 1 FROM item i WHERE i.target_repo = p.target_repo AND i.target_name = p.name
                               AND i.status IN ('pending', 'running'))
             ORDER BY p.target_repo, p.name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(io)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let target_repo: String = r.get("target_repo");
            let name: String = r.get("name");
            let format: String = r.get("format");
            let landed_rows = sqlx::query(
                "SELECT planned_json FROM item WHERE target_repo = ? AND target_name = ?
                   AND status IN ('copied', 'skipped') ORDER BY version",
            )
            .bind(&target_repo)
            .bind(&name)
            .fetch_all(&self.pool)
            .await
            .map_err(io)?;
            let mut landed = Vec::new();
            for l in landed_rows {
                let p: Planned = from_json(l.get::<&str, _>("planned_json"))?;
                landed.push((p.item.coord.version.clone(), p.item.extra.clone()));
            }
            out.push(Unsealed {
                format: crate::domain::Format::from_str(&format)
                    .map_err(|e| JournalError::Incompatible(e.to_string()))?,
                extra: from_json::<PkgExtra>(r.get::<&str, _>("extra_json"))?,
                target_repo,
                name,
                landed,
            });
        }
        Ok(out)
    }

    async fn sealed(&self, target_repo: &str, name: &str, gaps: &[Gap]) -> Result<(), JournalError> {
        let scope = format!("seal:{target_repo}/{name}");
        let mut tx = self.pool.begin().await.map_err(io)?;
        sqlx::query("DELETE FROM gap WHERE scope = ?").bind(&scope).execute(&mut *tx).await.map_err(io)?;
        insert_gaps(&mut tx, &scope, gaps).await.map_err(io)?;
        sqlx::query("UPDATE pkg SET sealed = 1 WHERE target_repo = ? AND name = ?")
            .bind(target_repo)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        tx.commit().await.map_err(io)
    }

    async fn items(&self) -> Result<Vec<Journaled>, JournalError> {
        let rows = sqlx::query("SELECT * FROM item ORDER BY target_repo, target_name, version, source_ref")
            .fetch_all(&self.pool)
            .await
            .map_err(io)?;
        rows.iter().map(journaled).collect()
    }

    async fn gaps(&self) -> Result<Vec<Gap>, JournalError> {
        let rows = sqlx::query("SELECT kind, source_ref, detail FROM gap ORDER BY kind, source_ref, detail")
            .fetch_all(&self.pool)
            .await
            .map_err(io)?;
        rows.iter().map(gap_of).collect()
    }

    async fn pending(&self) -> Result<u64, JournalError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM item WHERE status IN ('pending', 'running')")
            .fetch_one(&self.pool)
            .await
            .map_err(io)?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::import::{Coord, Digests, Item, Origin, SourceFormat, VersionExtra};

    fn planned(source_ref: &str, name: &str, version: &str, format: crate::domain::Format) -> Planned {
        Planned {
            item: Item {
                source_ref: source_ref.into(),
                format: SourceFormat::Npm,
                coord: Coord { repo: "r".into(), name: name.into(), version: version.into() },
                published_at: None,
                size: None,
                want: Digests::default(),
                origin: Origin::Npm { registry: "https://src.example/".into(), package: name.into() },
                pkg: PkgExtra::default(),
                extra: VersionExtra { yanked: version == "1.0.0" },
            },
            target_repo: "t".into(),
            target_name: name.into(),
            target_format: format,
        }
    }

    fn header() -> RunHeader {
        RunHeader {
            source: "verdaccio".into(),
            source_url: "https://src.example/".into(),
            target_url: "https://dst.example/".into(),
            opts_json: "{}".into(),
            started_at: Utc::now(),
            finished_at: None,
            phase: "running".into(),
            owner: None,
        }
    }

    async fn journal(dir: &tempfile::TempDir) -> SqliteImportJournal {
        SqliteImportJournal::open(&dir.path().join("s/state.db"), true).await.unwrap()
    }

    #[tokio::test]
    async fn claim_is_exclusive_under_four_workers() {
        let dir = tempfile::tempdir().unwrap();
        let j = std::sync::Arc::new(journal(&dir).await);
        let items: Vec<_> = (0..40)
            .map(|i| planned(&format!("ref{i}"), &format!("p{i}"), "1.0.1", crate::domain::Format::Npm))
            .collect();
        j.record("s", true, &items, &[], &None, true).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let j = j.clone();
            tasks.spawn(async move {
                let mut got = Vec::new();
                while let Some(it) = j.claim(Lane::File, Utc::now()).await.unwrap() {
                    got.push(it.planned.item.source_ref);
                }
                got
            });
        }
        let mut all = Vec::new();
        while let Some(r) = tasks.join_next().await {
            all.extend(r.unwrap());
        }
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 40);
    }

    #[tokio::test]
    async fn two_lanes_claim_disjoint_rows() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(&dir).await;
        let items = vec![
            planned("a", "a", "1", crate::domain::Format::Npm),
            planned("b", "b", "latest", crate::domain::Format::Oci),
        ];
        j.record("s", true, &items, &[], &None, true).await.unwrap();
        let blob = j.claim(Lane::Blob, Utc::now()).await.unwrap().unwrap();
        assert_eq!(blob.planned.item.source_ref, "b");
        assert!(j.claim(Lane::Blob, Utc::now()).await.unwrap().is_none());
        let file = j.claim(Lane::File, Utc::now()).await.unwrap().unwrap();
        assert_eq!(file.planned.item.source_ref, "a");
        assert!(j.claim(Lane::File, Utc::now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn user_version_mismatch_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let j = SqliteImportJournal::open(&path, true).await.unwrap();
        sqlx::raw_sql("PRAGMA user_version = 99").execute(&j.pool).await.unwrap();
        j.close().await;
        let err = SqliteImportJournal::open(&path, true).await.err().unwrap();
        assert!(matches!(err, JournalError::Incompatible(_)), "{err:?}");
    }

    #[tokio::test]
    async fn origin_with_a_query_or_userinfo_is_refused_at_persist_time() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(&dir).await;
        let mut p = planned("a", "a", "1", crate::domain::Format::Npm);
        p.item.origin = Origin::Asset {
            endpoint: "https://nexus.example/".into(),
            repo: "r".into(),
            path: "https://s3.example/x?X-Amz-Signature=abc".into(),
        };
        let err = j.record("s", true, &[p.clone()], &[], &None, true).await.err().unwrap();
        assert!(matches!(err, JournalError::Unredacted(_)));
        p.item.origin = Origin::Npm { registry: "https://u:p@src.example/".into(), package: "a".into() };
        assert!(j.record("s", true, &[p], &[], &None, true).await.is_err());
        assert!(j.items().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_second_live_owner_is_refused_and_a_stale_one_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(&dir).await;
        let now = Utc::now();
        j.begin(&header(), "host:1", true, now).await.unwrap();
        let err = j.begin(&header(), "host:2", false, now).await.err().unwrap();
        assert_eq!(err, JournalError::Busy("host:1".into()));
        let later = now + chrono::Duration::seconds(STALE_OWNER_SECS + 1);
        j.begin(&header(), "host:2", false, later).await.unwrap();
    }

    #[tokio::test]
    async fn a_restarted_stream_drops_its_own_gaps_and_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(&dir).await;
        let g = |s: &str| Gap::new(GapKind::NoTarget, s, "no target repository");
        j.record("s1", true, &[], &[g("x")], &None, true).await.unwrap();
        j.record("s2", true, &[], &[g("y"), g("y")], &None, true).await.unwrap();
        j.record("s1", true, &[], &[], &None, true).await.unwrap();
        assert_eq!(j.gaps().await.unwrap(), vec![g("y")]);
    }

    #[tokio::test]
    async fn a_package_is_unsealed_until_every_version_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let j = journal(&dir).await;
        let items = vec![
            planned("a1", "a", "1.0.0", crate::domain::Format::Cargo),
            planned("a2", "a", "1.1.0", crate::domain::Format::Cargo),
        ];
        j.record("s", true, &items, &[], &None, true).await.unwrap();
        let done = |status| Outcome { status, bytes: None, sha256: None, error: None, note: None, gaps: Vec::new() };
        let first = j.claim(Lane::File, Utc::now()).await.unwrap().unwrap();
        j.complete(&first.planned.item.source_ref, &done(ItemStatus::Copied)).await.unwrap();
        assert!(j.unsealed().await.unwrap().is_empty());
        let second = j.claim(Lane::File, Utc::now()).await.unwrap().unwrap();
        j.complete(&second.planned.item.source_ref, &done(ItemStatus::Skipped)).await.unwrap();
        let u = j.unsealed().await.unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(
            u[0].landed,
            vec![("1.0.0".into(), VersionExtra { yanked: true }), ("1.1.0".into(), VersionExtra { yanked: false })]
        );
        j.sealed("t", "a", &[]).await.unwrap();
        assert!(j.unsealed().await.unwrap().is_empty());
    }
}
