use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{DomainError, Format, RepoKind, Visibility};

/// The structured half of a repository's `config` document: the member list a
/// group resolves through, plus whatever else the document carried, so a
/// round-trip through an update cannot drop a key this type does not model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl RepoConfig {
    /// Tolerant on purpose, and the tolerance is the point: bad JSON, a
    /// missing `members` key and non-string entries all mean "not a group",
    /// because a repository whose config is garbage is exactly the one an
    /// admin call has to load in order to fix it.
    pub fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a string-keyed map always serializes")
    }

    pub fn of_members(members: &[String]) -> Self {
        Self {
            members: members.to_vec(),
            extra: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub repo_type: String,
    pub format: String,
    pub visibility: Visibility,
    pub upstream_url: Option<String>,
    /// `None` is not `Some(RepoConfig::default())`: the column is NULL on
    /// every hosted and every proxy repository, and an empty member list is a
    /// group with no members.
    pub config: Option<RepoConfig>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Repository {
    pub fn kind(&self) -> Result<RepoKind, DomainError> {
        self.repo_type
            .parse()
            .map_err(|_| self.corrupt_column("repo_type", &self.repo_type))
    }

    pub fn fmt(&self) -> Result<Format, DomainError> {
        self.format
            .parse()
            .map_err(|_| self.corrupt_column("format", &self.format))
    }

    pub fn members(&self) -> Vec<String> {
        self.config
            .as_ref()
            .map(|config| config.members.clone())
            .unwrap_or_default()
    }

    fn corrupt_column(&self, column: &'static str, value: &str) -> DomainError {
        DomainError::CorruptColumn {
            repo: self.name.clone(),
            column,
            value: value.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub id: i64,
    pub repository_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub readme: Option<String>,
    pub license: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub id: i64,
    pub package_id: i64,
    pub version: String,
    pub metadata_json: String,
    pub checksum_sha1: Option<String>,
    pub checksum_sha256: Option<String>,
    pub integrity: Option<String>,
    pub size: i64,
    pub tarball_path: String,
    pub published_at: DateTime<Utc>,
    pub yanked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistTag {
    pub id: i64,
    pub package_id: i64,
    pub tag: String,
    pub version_id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(repo_type: &str, format: &str) -> Repository {
        Repository {
            id: 1,
            name: "r".to_string(),
            repo_type: repo_type.to_string(),
            format: format.to_string(),
            visibility: Visibility::Public,
            upstream_url: None,
            config: Some(RepoConfig::of_members(&["a".to_string(), "b".to_string()])),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn kind_format_and_members_read_the_row() {
        let repo = repository("group", "cargo");
        assert_eq!(repo.kind().unwrap(), RepoKind::Group);
        assert_eq!(repo.fmt().unwrap(), Format::Cargo);
        assert_eq!(repo.members(), vec!["a".to_string(), "b".to_string()]);

        assert!(matches!(
            repository("mirror", "npm").kind(),
            Err(DomainError::CorruptColumn { .. })
        ));
        assert!(matches!(
            repository("hosted", "deb").fmt(),
            Err(DomainError::CorruptColumn { .. })
        ));
    }

    /// A repository with no config is not a group; an unreadable config is
    /// not a group either, and never an error -- the admin call that would
    /// repair the column has to be able to load the row.
    #[test]
    fn an_unreadable_config_is_not_a_group() {
        let mut repo = repository("group", "npm");
        repo.config = None;
        assert!(repo.members().is_empty());

        for garbage in [
            "not json",
            "[]",
            "{}",
            r#"{"members":"h"}"#,
            r#"{"members":[1]}"#,
        ] {
            repo.config = Some(RepoConfig::from_json(garbage));
            assert!(repo.members().is_empty(), "{garbage}");
        }
    }

    /// The stored text and the `config` field of the admin API are the same
    /// bytes, so an unmodelled key survives a read-modify-write.
    #[test]
    fn a_config_round_trips_through_its_json_keeping_unmodelled_keys() {
        let raw = r#"{"members":["h","p6"],"note":"kept"}"#;
        let config = RepoConfig::from_json(raw);
        assert_eq!(config.members, vec!["h".to_string(), "p6".to_string()]);
        assert_eq!(config.to_json(), raw);
        assert_eq!(
            RepoConfig::of_members(&["h".to_string(), "p6".to_string()]).to_json(),
            r#"{"members":["h","p6"]}"#
        );
    }
}
