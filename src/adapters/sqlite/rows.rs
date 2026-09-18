//! The stored encoding of every domain type this adapter reads back.
//!
//! One row struct per domain type, holding what SQLite actually keeps: a text
//! timestamp, a text visibility, a `config_json` document, an integer
//! `yanked`. Decoding them is the only place a stored timestamp is parsed,
//! and a column the schema was supposed to constrain coming back unreadable
//! is a `CorruptColumn`, never a silent fallback.

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::domain::{DistTag, DomainError, Package, RepoConfig, Repository, Version};

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
pub(crate) fn parse_ts(stored: &str) -> Option<DateTime<Utc>> {
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

/// The adapter's stored spelling of an instant: the one form every statement
/// binds, so no column default ever fires and every row carries the caller's
/// clock.
pub(crate) fn bind_ts(at: DateTime<Utc>) -> String {
    at.format(TS_FORMAT).to_string()
}

impl TryFrom<RepositoryRow> for Repository {
    type Error = DomainError;

    fn try_from(row: RepositoryRow) -> Result<Self, DomainError> {
        Ok(Repository {
            visibility: row
                .visibility
                .parse()
                .map_err(|_| corrupt(&row.name, "visibility", &row.visibility))?,
            created_at: super::read_ts(&row.name, "created_at", &row.created_at)?,
            updated_at: super::read_ts(&row.name, "updated_at", &row.updated_at)?,
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
            created_at: super::read_ts(&row.name, "created_at", &row.created_at)?,
            updated_at: super::read_ts(&row.name, "updated_at", &row.updated_at)?,
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
            published_at: super::read_ts(&row.version, "published_at", &row.published_at)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Visibility;

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
