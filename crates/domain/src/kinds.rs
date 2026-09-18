use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::DomainError;

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
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        RepoKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("invalid repository type: {s}")))
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
    Maven,
    Nuget,
}

impl Format {
    pub const ALL: [Format; 7] = [
        Format::Npm,
        Format::Cargo,
        Format::Oci,
        Format::Go,
        Format::Pypi,
        Format::Maven,
        Format::Nuget,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Npm => "npm",
            Format::Cargo => "cargo",
            Format::Oci => "oci",
            Format::Go => "go",
            Format::Pypi => "pypi",
            Format::Maven => "maven",
            Format::Nuget => "nuget",
        }
    }

    pub const fn osv_ecosystem(self) -> Option<&'static str> {
        match self {
            Format::Npm => Some("npm"),
            Format::Cargo => Some("crates.io"),
            Format::Go => Some("Go"),
            Format::Pypi => Some("PyPI"),
            Format::Maven => Some("Maven"),
            Format::Nuget => Some("NuGet"),
            Format::Oci => None,
        }
    }

    pub const fn supports_kind(self, _kind: RepoKind) -> bool {
        true
    }
}

impl FromStr for Format {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        Format::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("invalid repository format: {s}")))
    }
}

/// Who may read a repository. Anonymous reads of a public one are gated
/// separately, by the server's `anonymous_read` switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    #[default]
    Private,
}

impl Visibility {
    pub const ALL: [Visibility; 2] = [Visibility::Public, Visibility::Private];

    pub const fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }
}

impl FromStr for Visibility {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        Visibility::ALL
            .into_iter()
            .find(|visibility| visibility.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("invalid visibility: {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// That these values are the ones the `repo_type` and `format` CHECKs admit
    /// is asserted by the SQLite adapter against a migrated database
    /// (`adapters/sqlite/migrate_tests.rs`): the dialect is the adapter's to
    /// know, and reading a migration file from here would only prove that two
    /// texts agree.
    #[test]
    fn values_round_trip_and_predicates_hold() {
        for kind in RepoKind::ALL {
            assert_eq!(kind.as_str().parse::<RepoKind>().unwrap(), kind);
        }

        for format in Format::ALL {
            assert_eq!(format.as_str().parse::<Format>().unwrap(), format);
            for kind in RepoKind::ALL {
                assert!(format.supports_kind(kind), "{format:?}/{kind:?}");
            }
        }
        assert_eq!(Format::Cargo.osv_ecosystem(), Some("crates.io"));
        assert_eq!(Format::Oci.osv_ecosystem(), None);
        assert_eq!(Format::Pypi.osv_ecosystem(), Some("PyPI"));
        assert_eq!(Format::Maven.osv_ecosystem(), Some("Maven"));
        assert_eq!(Format::Nuget.osv_ecosystem(), Some("NuGet"));

        for visibility in Visibility::ALL {
            assert_eq!(
                visibility.as_str().parse::<Visibility>().unwrap(),
                visibility
            );
        }

        assert!(matches!(
            "mirror".parse::<RepoKind>(),
            Err(DomainError::InvalidName(_))
        ));
        assert!(matches!(
            "deb".parse::<Format>(),
            Err(DomainError::InvalidName(_))
        ));
        assert!(matches!(
            "secret".parse::<Visibility>(),
            Err(DomainError::InvalidName(_))
        ));
    }

    /// The four live assertions on `"visibility"` bodies read the lowercase
    /// name, and `Visibility` reaches them through `serde` now that the column
    /// is typed.
    #[test]
    fn visibility_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Visibility::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::to_string(&Visibility::Private).unwrap(),
            "\"private\""
        );
    }
}
