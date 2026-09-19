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

    /// Whether every version this format calls a pre-release carries a
    /// hyphen, which lets a store narrow its scan before the predicate
    /// decides. False for PEP 440, where `1.0rc1` has none.
    pub const fn prerelease_implies_hyphen(self) -> bool {
        match self {
            Format::Npm | Format::Cargo | Format::Nuget => true,
            Format::Pypi => false,
            // No pre-release at all, so the implication holds vacuously.
            Format::Go | Format::Maven | Format::Oci | Format::Mcp | Format::Raw => true,
        }
    }

    /// Whether this format calls `version` a pre-release. A format whose row
    /// says `No` never does, so the retention sweep cannot reach it.
    pub fn is_prerelease(self, version: &str) -> bool {
        match self {
            // SemVer: everything after the first hyphen is the pre-release.
            Format::Npm | Format::Cargo | Format::Nuget => version.contains('-'),
            // PEP 440: a, b, rc and dev segments, with or without separators.
            Format::Pypi => pep440_prerelease(version),
            Format::Go
            | Format::Maven
            | Format::Oci
            | Format::Mcp
            | Format::Raw => false,
        }
    }
}

/// PEP 440 spells a pre-release `1.0a1`, `1.0.b2`, `1.0-rc1` or `1.0.dev3`,
/// and accepts `alpha`, `beta`, `c`, `pre` and `preview` as aliases. A `post`
/// release is not one, and the local segment after `+` is not read: `1.0+abc1`
/// is a final release built somewhere.
fn pep440_prerelease(version: &str) -> bool {
    const MARKERS: [&str; 9] = ["alpha", "beta", "preview", "pre", "rc", "dev", "a", "b", "c"];
    let lower = version.to_ascii_lowercase();
    let public = lower.split('+').next().unwrap_or_default();
    for (at, _) in public.char_indices().filter(|(_, c)| c.is_ascii_alphabetic()) {
        let tail = &public[at..];
        for marker in MARKERS {
            let Some(after) = tail.strip_prefix(marker) else {
                continue;
            };
            let after = after.trim_start_matches(['-', '_', '.']);
            if after.is_empty() || after.starts_with(|c: char| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
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

/// A `No` carries its reason so an absence is a decision, not an oversight.
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
    /// Exhaustive on purpose: a tenth format does not compile until every
    /// column has an answer.
    pub const fn coverage(self) -> Coverage {
        const NO_DOC: &str = "no package document: nothing declares dependencies";
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
                metered_publish: Cell::No(
                    "a deploy is a file per request, with none that completes it",
                ),
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

    /// The compiler refuses a missing row; this refuses a silent `No`.
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

    /// PyPI named an ecosystem, read no dependency, and answered clean forever.
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

#[cfg(test)]
mod prerelease_tests {
    use super::*;

    /// A format that declares the notion recognises its own spellings, and a
    /// format that declares none says no to all of them. The sweep reads this,
    /// so a `Yes` with a predicate that never fires would delete nothing and
    /// look healthy.
    #[test]
    fn each_format_reads_the_versions_it_calls_pre_releases() {
        for format in Format::ALL {
            let spellings: &[&str] = match format {
                Format::Npm | Format::Cargo | Format::Nuget => &["1.0.0-beta", "2.0.0-rc.1"],
                Format::Pypi => &["1.0a1", "1.0.b2", "1.0-rc1", "1.0.dev3", "2.0alpha"],
                _ => &[],
            };
            for version in spellings {
                assert!(
                    format.is_prerelease(version),
                    "{format:?} does not read {version} as a pre-release"
                );
            }
            if !format.coverage().prerelease.yes() {
                for version in ["1.0.0-beta", "1.0a1", "v0.0.0-20200101000000-abcdef"] {
                    assert!(
                        !format.is_prerelease(version),
                        "{format:?} says no to the notion and yes to {version}"
                    );
                }
            }
            for stable in ["1.0.0", "2.1.3"] {
                assert!(!format.is_prerelease(stable), "{format:?}: {stable}");
            }
        }
    }

    /// The three that would cost a user an artifact. A Go pseudo-version
    /// carries a hyphen and is permanent; a PyPI post release is a release;
    /// a local segment is where a build writes its own name.
    #[test]
    fn the_lookalikes_are_not_pre_releases() {
        assert!(!Format::Go.is_prerelease("v0.0.0-20200101000000-abcdef"));
        assert!(!Format::Pypi.is_prerelease("1.0.post1"));
        assert!(!Format::Pypi.is_prerelease("1.0+abc1"));
        assert!(!Format::Pypi.is_prerelease("1.0+ubuntu22.04"));
        assert!(!Format::Nuget.is_prerelease("1.0.0"));
        assert!(!Format::Maven.is_prerelease("1.0-SNAPSHOT"));
    }

    /// The hint a store pre-filters on, held against the predicate that
    /// decides: a format claiming the implication must never call a
    /// hyphen-free version a pre-release, or the narrowed scan loses it.
    #[test]
    fn the_hyphen_hint_never_drops_what_the_predicate_would_keep() {
        for format in Format::ALL.into_iter().filter(|f| f.prerelease_implies_hyphen()) {
            for version in ["1.0.0", "1.0.0+build", "2024.1.1", "1.0.0rc1", "1.0a1", "1.0.dev3"] {
                assert!(
                    !format.is_prerelease(version),
                    "{format:?} claims every pre-release carries a hyphen and calls \
                     {version} one, so a hyphen pre-filter would lose it"
                );
            }
        }
        assert!(
            !Format::Pypi.prerelease_implies_hyphen(),
            "PEP 440 spells 1.0rc1 without one, so PyPI cannot be narrowed that way"
        );
    }
}
