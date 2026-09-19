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
    Mcp,
    Raw,
}

impl Format {
    pub const ALL: [Format; 9] = [
        Format::Npm,
        Format::Cargo,
        Format::Oci,
        Format::Go,
        Format::Pypi,
        Format::Maven,
        Format::Nuget,
        Format::Mcp,
        Format::Raw,
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
            Format::Mcp => "mcp",
            Format::Raw => "raw",
        }
    }

    /// Whether a publish of this format has one request the meter counts.
    /// A Maven deploy is a file per request with none that completes it,
    /// so a request count would not be an artifact count; an OCI push is
    /// counted at its manifest put, the request that makes the image exist.
    pub const fn metered_publish(self) -> bool {
        self.coverage().metered_publish.yes()
    }

    pub const fn osv_ecosystem(self) -> Option<&'static str> {
        match self {
            Format::Npm => Some("npm"),
            Format::Cargo => Some("crates.io"),
            Format::Go => Some("Go"),
            Format::Pypi => Some("PyPI"),
            Format::Maven => Some("Maven"),
            Format::Nuget => Some("NuGet"),
            Format::Oci | Format::Mcp | Format::Raw => None,
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

/// What a format answers for one of the lists that enumerate formats.
///
/// A list nobody holds is how eight defects reached `main` at once: a format
/// arrives, a dozen lists must learn it, and the ones that are a `Vec` rather
/// than a `match` stay silent. Every such list is a column here, every format
/// a row, and `No` carries the reason so an absence is a decision on the
/// record rather than an oversight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Yes,
    No(&'static str),
}

impl Cell {
    pub const fn yes(self) -> bool {
        matches!(self, Cell::Yes)
    }

    pub const fn why(self) -> Option<&'static str> {
        match self {
            Cell::No(reason) => Some(reason),
            Cell::Yes => None,
        }
    }
}

/// One row of the matrix. Adding a column here makes every format answer it;
/// adding a format makes `Format::coverage` fail to compile until it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// A publish has one request the meter counts.
    pub metered_publish: Cell,
    /// A publish is judged by the policy gate before anything is written.
    pub publish_gate: Cell,
    /// A proxy resolution is recorded for the policy report.
    pub policy_record: Cell,
    /// A published version's dependencies can be read for a vulnerability scan.
    pub scannable: Cell,
    /// Deleting the repository counts this format's rows before it tries.
    pub emptiness_probe: Cell,
    /// This format's keys are in the reclaimer's reference union.
    pub reclaim_referenced: Cell,
    /// A download is recorded against the version it served.
    pub download_signal: Cell,
    /// "Pre-release" is a notion this format defines.
    pub prerelease: Cell,
}

impl Format {
    /// The matrix, one row per format. Exhaustive on purpose: a tenth format
    /// does not compile until every column has an answer.
    pub const fn coverage(self) -> Coverage {
        const NO_DOC: &str = "no package document: nothing declares dependencies";
        const SESSION: &str = "a push is a session, not one request that completes it";
        match self {
            Format::Npm | Format::Cargo | Format::Nuget => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::Yes,
                policy_record: Cell::Yes,
                scannable: Cell::Yes,
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::Yes,
                prerelease: Cell::Yes,
            },
            Format::Go => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::Yes,
                policy_record: Cell::Yes,
                scannable: Cell::Yes,
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::Yes,
                prerelease: Cell::No(
                    "a pseudo-version carries a hyphen while being permanent, so the two \
                     cannot be told apart",
                ),
            },
            Format::Pypi => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::Yes,
                policy_record: Cell::Yes,
                scannable: Cell::Yes,
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::Yes,
                prerelease: Cell::Yes,
            },
            Format::Maven => Coverage {
                metered_publish: Cell::No(SESSION),
                publish_gate: Cell::Yes,
                policy_record: Cell::Yes,
                scannable: Cell::Yes,
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::No("no read path records against a version"),
                prerelease: Cell::No("a snapshot is not a pre-release"),
            },
            Format::Oci => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::Yes,
                policy_record: Cell::Yes,
                scannable: Cell::No(NO_DOC),
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::No("a pull is a manifest and blobs, not a version"),
                prerelease: Cell::No("a tag carrying a hyphen is a tag, not a pre-release"),
            },
            Format::Mcp => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::No("governance judges a server, the policy engine does not"),
                policy_record: Cell::Yes,
                scannable: Cell::No(NO_DOC),
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::No("a catalog read is not a download"),
                prerelease: Cell::No("a server version is not semver by contract"),
            },
            Format::Raw => Coverage {
                metered_publish: Cell::Yes,
                publish_gate: Cell::No("a file declares nothing a rule could judge"),
                policy_record: Cell::Yes,
                scannable: Cell::No(NO_DOC),
                emptiness_probe: Cell::Yes,
                reclaim_referenced: Cell::Yes,
                download_signal: Cell::No("a path is not a version"),
                prerelease: Cell::No("a path carries no version"),
            },
        }
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
        assert_eq!(Format::Mcp.osv_ecosystem(), None, "the package behind a server is scanned in its own format");
        assert_eq!(Format::Raw.osv_ecosystem(), None);

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

#[cfg(test)]
mod coverage_tests {
    use super::*;

    /// Every cell of every row is stated. The compiler already refuses a
    /// missing row; this refuses a row that answers `No` with nothing to say,
    /// because "no" without a reason is the oversight the matrix exists to end.
    #[test]
    fn every_absence_carries_its_reason() {
        for format in Format::ALL {
            let c = format.coverage();
            for (column, cell) in [
                ("metered_publish", c.metered_publish),
                ("publish_gate", c.publish_gate),
                ("policy_record", c.policy_record),
                ("scannable", c.scannable),
                ("emptiness_probe", c.emptiness_probe),
                ("reclaim_referenced", c.reclaim_referenced),
                ("download_signal", c.download_signal),
                ("prerelease", c.prerelease),
            ] {
                if let Some(why) = cell.why() {
                    assert!(
                        why.len() > 20,
                        "{format:?}.{column} says no without saying why: {why:?}"
                    );
                }
            }
        }
    }

    /// The column a format declares and the ecosystem it names must agree: a
    /// format that says it is scannable has somewhere to send the scan, and
    /// one that names an ecosystem is scanned. PyPI held both halves apart —
    /// it named `PyPI` and read no dependency — and answered clean forever.
    #[test]
    fn scannable_and_its_ecosystem_are_one_statement() {
        for format in Format::ALL {
            assert_eq!(
                format.coverage().scannable.yes(),
                format.osv_ecosystem().is_some(),
                "{format:?}: scannable and osv_ecosystem disagree"
            );
        }
    }
}
