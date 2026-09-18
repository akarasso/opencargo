use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::SqlitePool;
use tracing::info;

use crate::domain::{
    DistTag, DomainError, Format, Package, RepoConfig, RepoKind, Repository, Version, Visibility,
};
use crate::error::AppResult;

pub mod kinds;
pub mod oci;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------
//
// One per domain type, holding the SQLite encoding of every column the domain
// states as a value: a text timestamp, a text visibility, a `config_json`
// document, an integer `yanked`. Decoding them is the only place a stored
// timestamp is parsed, and a column the schema was supposed to constrain
// coming back unreadable is a `CorruptColumn`, never a silent fallback.

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RepositoryRow {
    pub id: i64,
    pub name: String,
    pub repo_type: String,
    pub format: String,
    pub visibility: String,
    pub upstream_url: Option<String>,
    pub config_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct PackageRow {
    pub id: i64,
    pub repository_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub license: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct VersionRow {
    pub id: i64,
    pub package_id: i64,
    pub version: String,
    pub metadata_json: String,
    pub checksum_sha1: Option<String>,
    pub checksum_sha256: Option<String>,
    pub integrity: Option<String>,
    pub size: i64,
    pub tarball_path: String,
    pub published_at: String,
    pub yanked: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub struct DistTagRow {
    pub id: i64,
    pub package_id: i64,
    pub tag: String,
    pub version_id: i64,
}

fn corrupt(subject: &str, column: &'static str, value: &str) -> DomainError {
    DomainError::CorruptColumn {
        repo: subject.to_string(),
        column,
        value: value.to_string(),
    }
}

/// A stored timestamp back as a value. `bind_ts`'s inverse, and tolerant of
/// the RFC 3339 a hand-written row or another dialect may carry.
pub fn parse_ts(stored: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(stored, TS_FORMAT)
        .map(|naive| naive.and_utc())
        .ok()
        .or_else(|| {
            DateTime::parse_from_rfc3339(stored)
                .map(|at| at.with_timezone(&Utc))
                .ok()
        })
}

/// The adapter's stored timestamp: UTC, second precision. Two live predicates
/// compare these strings lexicographically against `datetime('now')`, and
/// `'T'` sorts above `' '`, so an RFC 3339 write would mis-evaluate every
/// legacy row.
const TS_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The one spelling every statement binds, so no column default ever fires
/// and every row carries the caller's clock. It lives beside `parse_ts`, its
/// inverse, until the last free function here has become a store and the
/// whole codec moves into the SQLite adapter with them.
pub fn bind_ts(at: DateTime<Utc>) -> String {
    at.format(TS_FORMAT).to_string()
}

fn ts(subject: &str, column: &'static str, stored: &str) -> Result<DateTime<Utc>, DomainError> {
    parse_ts(stored).ok_or_else(|| corrupt(subject, column, stored))
}

impl TryFrom<RepositoryRow> for Repository {
    type Error = DomainError;

    fn try_from(row: RepositoryRow) -> Result<Self, DomainError> {
        Ok(Repository {
            visibility: row
                .visibility
                .parse()
                .map_err(|_| corrupt(&row.name, "visibility", &row.visibility))?,
            created_at: ts(&row.name, "created_at", &row.created_at)?,
            updated_at: ts(&row.name, "updated_at", &row.updated_at)?,
            config: row.config_json.as_deref().map(RepoConfig::from_json),
            id: row.id,
            name: row.name,
            repo_type: row.repo_type,
            format: row.format,
            upstream_url: row.upstream_url,
        })
    }
}

impl TryFrom<PackageRow> for Package {
    type Error = DomainError;

    fn try_from(row: PackageRow) -> Result<Self, DomainError> {
        Ok(Package {
            created_at: ts(&row.name, "created_at", &row.created_at)?,
            updated_at: ts(&row.name, "updated_at", &row.updated_at)?,
            id: row.id,
            repository_id: row.repository_id,
            name: row.name,
            description: row.description,
            readme: row.readme,
            license: row.license,
        })
    }
}

impl TryFrom<VersionRow> for Version {
    type Error = DomainError;

    fn try_from(row: VersionRow) -> Result<Self, DomainError> {
        Ok(Version {
            published_at: ts(&row.version, "published_at", &row.published_at)?,
            yanked: row.yanked != 0,
            id: row.id,
            package_id: row.package_id,
            version: row.version,
            metadata_json: row.metadata_json,
            checksum_sha1: row.checksum_sha1,
            checksum_sha256: row.checksum_sha256,
            integrity: row.integrity,
            size: row.size,
            tarball_path: row.tarball_path,
        })
    }
}

impl From<DistTagRow> for DistTag {
    fn from(row: DistTagRow) -> Self {
        DistTag {
            id: row.id,
            package_id: row.package_id,
            tag: row.tag,
            version_id: row.version_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Connection & migration
// ---------------------------------------------------------------------------

/// Create a connection pool and enable WAL mode.
pub async fn connect(url: &str) -> anyhow::Result<SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
    use std::str::FromStr;

    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // Wait for a busy writer instead of failing immediately: instant
        // SQLITE_BUSY errors under load used to surface as spurious 401s in
        // the auth middleware.
        .busy_timeout(std::time::Duration::from_secs(5))
        // SQLite leaves foreign-key enforcement OFF per connection unless
        // asked; the schema declares FK constraints and relies on them.
        .pragma("foreign_keys", "ON");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    info!("Connected to SQLite database");
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Query functions
// ---------------------------------------------------------------------------

pub async fn get_repository_by_name(pool: &SqlitePool, name: &str) -> AppResult<Option<Repository>> {
    let row: Option<RepositoryRow> =
        sqlx::query_as("SELECT * FROM repositories WHERE name = ?1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(Repository::try_from).transpose()?)
}

pub async fn create_package(
    pool: &SqlitePool,
    repo_id: i64,
    name: &str,
    description: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO packages (repository_id, name, description) VALUES (?1, ?2, ?3)",
    )
    .bind(repo_id)
    .bind(name)
    .bind(description)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

/// Update a package's stored README (raw markdown). The dashboard renders it
/// through `render_markdown` -> ammonia, so the raw source is stored as-is and
/// sanitized at display time.
pub async fn update_package_readme(
    pool: &SqlitePool,
    package_id: i64,
    readme: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE packages SET readme = ?1 WHERE id = ?2")
        .bind(readme)
        .bind(package_id)
        .execute(pool)
        .await?;
    Ok(())
}

// Mirrors the `versions` table columns; a parameter struct is the eventual
// cleanup but out of scope for the clippy pass.
#[allow(clippy::too_many_arguments)]
pub async fn create_version(
    pool: &SqlitePool,
    package_id: i64,
    version: &str,
    metadata_json: &str,
    checksum_sha1: Option<&str>,
    checksum_sha256: Option<&str>,
    integrity: Option<&str>,
    size: i64,
    tarball_path: &str,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO versions (package_id, version, metadata_json, checksum_sha1, checksum_sha256, integrity, size, tarball_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(package_id)
    .bind(version)
    .bind(metadata_json)
    .bind(checksum_sha1)
    .bind(checksum_sha256)
    .bind(integrity)
    .bind(size)
    .bind(tarball_path)
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

/// Replace the stored metadata JSON of a version (used by `npm deprecate`).
pub async fn update_version_metadata(
    pool: &SqlitePool,
    version_id: i64,
    metadata_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE versions SET metadata_json = ?1 WHERE id = ?2")
        .bind(metadata_json)
        .bind(version_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_dist_tag(
    pool: &SqlitePool,
    package_id: i64,
    tag: &str,
    version_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO dist_tags (package_id, tag, version_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(package_id, tag) DO UPDATE SET version_id = excluded.version_id",
    )
    .bind(package_id)
    .bind(tag)
    .bind(version_id)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_dist_tags(
    pool: &SqlitePool,
    package_id: i64,
) -> Result<Vec<DistTag>, sqlx::Error> {
    let rows: Vec<DistTagRow> = sqlx::query_as("SELECT * FROM dist_tags WHERE package_id = ?1")
        .bind(package_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(DistTag::from).collect())
}

pub async fn set_yanked(
    pool: &SqlitePool,
    version_id: i64,
    yanked: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE versions SET yanked = ?1 WHERE id = ?2")
        .bind(if yanked { 1i64 } else { 0i64 })
        .bind(version_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository CRUD
// ---------------------------------------------------------------------------

pub async fn get_all_repositories(pool: &SqlitePool) -> AppResult<Vec<Repository>> {
    let rows: Vec<RepositoryRow> = sqlx::query_as("SELECT * FROM repositories ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(Repository::try_from)
        .collect::<Result<_, _>>()?)
}

pub async fn create_repository(
    pool: &SqlitePool,
    name: &str,
    kind: RepoKind,
    format: Format,
    visibility: Visibility,
    upstream_url: Option<&str>,
    config: Option<&RepoConfig>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO repositories (name, repo_type, format, visibility, upstream_url, config_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(name)
    .bind(kind.as_str())
    .bind(format.as_str())
    .bind(visibility.as_str())
    .bind(upstream_url)
    .bind(config.map(RepoConfig::to_json))
    .execute(pool)
    .await?;

    Ok(result.last_insert_rowid())
}

pub async fn update_repository(
    pool: &SqlitePool,
    name: &str,
    visibility: Option<Visibility>,
    upstream_url: Option<&str>,
    config: Option<&RepoConfig>,
) -> Result<(), sqlx::Error> {
    if let Some(vis) = visibility {
        sqlx::query(
            "UPDATE repositories SET visibility = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(vis.as_str())
        .bind(name)
        .execute(pool)
        .await?;
    }
    if let Some(url) = upstream_url {
        sqlx::query(
            "UPDATE repositories SET upstream_url = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(url)
        .bind(name)
        .execute(pool)
        .await?;
    }
    if let Some(cfg) = config {
        sqlx::query(
            "UPDATE repositories SET config_json = ?1, updated_at = datetime('now') WHERE name = ?2",
        )
        .bind(cfg.to_json())
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn delete_repository(pool: &SqlitePool, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM repositories WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    /// A migrated (twice, proving idempotence) SQLite pool in a temp dir.
    pub async fn pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
        let pool = super::connect(&url).await.unwrap();
        crate::server::migrate(&pool).await.unwrap();
        crate::server::migrate(&pool).await.unwrap();
        (tmp, pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository_row() -> RepositoryRow {
        RepositoryRow {
            id: 1,
            name: "npm-hosted".to_string(),
            repo_type: "hosted".to_string(),
            format: "npm".to_string(),
            visibility: "private".to_string(),
            upstream_url: None,
            config_json: None,
            created_at: "2026-09-17 10:02:03".to_string(),
            updated_at: "2026-09-17 10:02:03".to_string(),
        }
    }

    #[test]
    fn a_row_decodes_every_column_the_domain_states_as_a_value() {
        let repo = Repository::try_from(repository_row()).unwrap();
        assert_eq!(repo.visibility, Visibility::Private);
        assert_eq!(repo.created_at.to_rfc3339(), "2026-09-17T10:02:03+00:00");
        assert_eq!(repo.config, None);

        let version = Version::try_from(VersionRow {
            id: 1,
            package_id: 1,
            version: "1.0.0".to_string(),
            metadata_json: "{}".to_string(),
            checksum_sha1: None,
            checksum_sha256: None,
            integrity: None,
            size: 1,
            tarball_path: String::new(),
            published_at: "2026-09-17 10:02:03".to_string(),
            yanked: 1,
        })
        .unwrap();
        assert!(version.yanked);
        assert_eq!(
            version.published_at.to_rfc3339(),
            "2026-09-17T10:02:03+00:00"
        );
    }

    /// The stored format is the adapter's, but a row written by hand or by a
    /// backend that keeps offsets still decodes rather than being refused.
    #[test]
    fn rfc_3339_is_read_back_too() {
        let mut row = repository_row();
        row.created_at = "2026-09-17T12:02:03+02:00".to_string();
        let repo = Repository::try_from(row).unwrap();
        assert_eq!(repo.created_at.to_rfc3339(), "2026-09-17T10:02:03+00:00");
    }

    /// A column the schema was supposed to constrain coming back unreadable
    /// is an error naming the column, never a fallback: the `go` `Time` field
    /// used to echo whatever it could not parse.
    #[test]
    fn an_unreadable_column_names_itself() {
        for (column, corrupt) in [
            ("visibility", "sometimes" as &str),
            ("created_at", "17/09/2026"),
        ] {
            let mut row = repository_row();
            match column {
                "visibility" => row.visibility = corrupt.to_string(),
                _ => row.created_at = corrupt.to_string(),
            }
            let err = Repository::try_from(row).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("repository 'npm-hosted' has a corrupt {column} column: '{corrupt}'")
            );
        }
    }
}
