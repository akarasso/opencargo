use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::db::Repository;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    #[default]
    Hosted,
    Proxy,
    Group,
}

impl RepoKind {
    pub const ALL: [RepoKind; 3] = [RepoKind::Hosted, RepoKind::Proxy, RepoKind::Group];

    pub const fn as_str(self) -> &'static str {
        match self {
            RepoKind::Hosted => "hosted",
            RepoKind::Proxy => "proxy",
            RepoKind::Group => "group",
        }
    }
}

impl FromStr for RepoKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, AppError> {
        RepoKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| AppError::BadRequest(format!("invalid repository type: {s}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Npm,
    Cargo,
    Oci,
    Go,
    Pypi,
}

impl Format {
    pub const ALL: [Format; 5] = [
        Format::Npm,
        Format::Cargo,
        Format::Oci,
        Format::Go,
        Format::Pypi,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Npm => "npm",
            Format::Cargo => "cargo",
            Format::Oci => "oci",
            Format::Go => "go",
            Format::Pypi => "pypi",
        }
    }

    pub const fn osv_ecosystem(self) -> Option<&'static str> {
        match self {
            Format::Npm => Some("npm"),
            Format::Cargo => Some("crates.io"),
            Format::Go => Some("Go"),
            Format::Oci | Format::Pypi => None,
        }
    }

    pub const fn supports_kind(self, kind: RepoKind) -> bool {
        !matches!((self, kind), (Format::Pypi, RepoKind::Proxy | RepoKind::Group))
    }
}

impl FromStr for Format {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, AppError> {
        Format::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| AppError::BadRequest(format!("invalid repository format: {s}")))
    }
}

/// Transitional rule until every format has an upstream strategy: proxy and
/// group repositories exist for npm only.
pub fn ensure_kind_supported(kind: RepoKind, format: Format) -> AppResult<()> {
    if format.supports_kind(kind) && (kind == RepoKind::Hosted || format == Format::Npm) {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} repositories are only supported for npm today; {} repositories must be hosted",
        kind.as_str(),
        format.as_str()
    )))
}

impl Repository {
    pub fn kind(&self) -> AppResult<RepoKind> {
        self.repo_type
            .parse()
            .map_err(|_| self.corrupt_column("repo_type", &self.repo_type))
    }

    pub fn fmt(&self) -> AppResult<Format> {
        self.format
            .parse()
            .map_err(|_| self.corrupt_column("format", &self.format))
    }

    pub fn members(&self) -> Vec<String> {
        super::parse_group_members(self.config_json.as_deref())
    }

    fn corrupt_column(&self, column: &str, value: &str) -> AppError {
        AppError::Internal(format!(
            "repository '{}' has a corrupt {column} column: '{value}'",
            self.name
        ))
    }
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
            visibility: "public".to_string(),
            upstream_url: None,
            config_json: Some(r#"{"members":["a","b"]}"#.to_string()),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// The values quoted inside `CHECK(<column> IN (...))` of the migration.
    fn check_values(column: &str) -> Vec<String> {
        let sql = include_str!("migrations/001_initial.sql");
        let line = sql
            .lines()
            .find(|l| l.contains(&format!("CHECK({column} IN (")))
            .expect("CHECK constraint present");
        let list = line.split("IN (").nth(1).unwrap().split(')').next().unwrap();
        list.split(',')
            .map(|v| v.trim().trim_matches('\'').to_string())
            .collect()
    }

    #[test]
    fn roundtrip_matches_check_constraints() {
        let kinds: Vec<String> = RepoKind::ALL.iter().map(|k| k.as_str().to_string()).collect();
        assert_eq!(kinds, check_values("repo_type"));
        for kind in RepoKind::ALL {
            assert_eq!(kind.as_str().parse::<RepoKind>().unwrap(), kind);
        }

        let formats: Vec<String> = Format::ALL.iter().map(|f| f.as_str().to_string()).collect();
        assert_eq!(formats, check_values("format"));
        for format in Format::ALL {
            assert_eq!(format.as_str().parse::<Format>().unwrap(), format);
            for kind in RepoKind::ALL {
                let expected = format != Format::Pypi || kind == RepoKind::Hosted;
                assert_eq!(format.supports_kind(kind), expected, "{format:?}/{kind:?}");
            }
        }
        assert_eq!(Format::Cargo.osv_ecosystem(), Some("crates.io"));
        assert_eq!(Format::Oci.osv_ecosystem(), None);

        let repo = repository("group", "cargo");
        assert_eq!(repo.kind().unwrap(), RepoKind::Group);
        assert_eq!(repo.fmt().unwrap(), Format::Cargo);
        assert_eq!(repo.members(), vec!["a".to_string(), "b".to_string()]);

        assert!(matches!("mirror".parse::<RepoKind>(), Err(AppError::BadRequest(_))));
        assert!(matches!(repository("mirror", "npm").kind(), Err(AppError::Internal(_))));
        assert!(matches!(repository("hosted", "deb").fmt(), Err(AppError::Internal(_))));
        assert!(matches!(
            ensure_kind_supported(RepoKind::Proxy, Format::Cargo),
            Err(AppError::BadRequest(_))
        ));
        assert!(ensure_kind_supported(RepoKind::Group, Format::Npm).is_ok());
        assert!(ensure_kind_supported(RepoKind::Hosted, Format::Pypi).is_ok());
    }
}
