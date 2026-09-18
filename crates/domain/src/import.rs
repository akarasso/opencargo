//! What a migration import decides about what it could not copy, and the
//! pure rules its plan applies before a byte moves.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{DomainError, Format};

/// Why a coordinate of the source is not, or not provably, in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GapKind {
    UnsupportedFormat,
    NoTarget,
    TargetCollision,
    CopiedUnverified,
    SkippedUnverifiable,
    TooLarge,
    ListingIncomplete,
    SourceOnlyFeature,
    PermissionNotMapped,
    TargetRefused,
    UnpublishableName,
    Failed,
}

/// The one class each gap kind belongs to; the run's exit code is the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Informational,
    Incomplete,
    Blocking,
}

pub const EXIT_CLEAN: u8 = 0;
pub const EXIT_NOT_STARTED: u8 = 1;
pub const EXIT_BLOCKING: u8 = 2;
pub const EXIT_ABORTED: u8 = 3;
pub const EXIT_INCOMPLETE: u8 = 4;

impl GapKind {
    pub const ALL: [GapKind; 12] = [
        GapKind::UnsupportedFormat,
        GapKind::NoTarget,
        GapKind::TargetCollision,
        GapKind::CopiedUnverified,
        GapKind::SkippedUnverifiable,
        GapKind::TooLarge,
        GapKind::ListingIncomplete,
        GapKind::SourceOnlyFeature,
        GapKind::PermissionNotMapped,
        GapKind::TargetRefused,
        GapKind::UnpublishableName,
        GapKind::Failed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            GapKind::UnsupportedFormat => "UnsupportedFormat",
            GapKind::NoTarget => "NoTarget",
            GapKind::TargetCollision => "TargetCollision",
            GapKind::CopiedUnverified => "CopiedUnverified",
            GapKind::SkippedUnverifiable => "SkippedUnverifiable",
            GapKind::TooLarge => "TooLarge",
            GapKind::ListingIncomplete => "ListingIncomplete",
            GapKind::SourceOnlyFeature => "SourceOnlyFeature",
            GapKind::PermissionNotMapped => "PermissionNotMapped",
            GapKind::TargetRefused => "TargetRefused",
            GapKind::UnpublishableName => "UnpublishableName",
            GapKind::Failed => "Failed",
        }
    }

    pub const fn class(self) -> ExitClass {
        match self {
            GapKind::Failed
            | GapKind::TargetRefused
            | GapKind::TooLarge
            | GapKind::CopiedUnverified
            | GapKind::UnpublishableName
            | GapKind::TargetCollision => ExitClass::Blocking,
            GapKind::UnsupportedFormat | GapKind::NoTarget | GapKind::ListingIncomplete => {
                ExitClass::Incomplete
            }
            GapKind::SkippedUnverifiable
            | GapKind::SourceOnlyFeature
            | GapKind::PermissionNotMapped => ExitClass::Informational,
        }
    }

    /// Item-derived kinds are rendered from the item they describe, so
    /// re-copying the item rewrites them; only the others are ever stored.
    pub const fn item_derived(self) -> bool {
        matches!(
            self,
            GapKind::Failed
                | GapKind::TooLarge
                | GapKind::TargetRefused
                | GapKind::CopiedUnverified
                | GapKind::SkippedUnverifiable
        )
    }
}

impl fmt::Display for GapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GapKind {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        GapKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("unknown gap kind: {s}")))
    }
}

/// Blocking outranks incomplete outranks informational; an operator who
/// accepted an incomplete source once, in writing, gets a clean exit for it.
pub fn exit_code(kinds: impl IntoIterator<Item = GapKind>, allow_incomplete: bool) -> u8 {
    let mut code = EXIT_CLEAN;
    for kind in kinds {
        match kind.class() {
            ExitClass::Blocking => return EXIT_BLOCKING,
            ExitClass::Incomplete if !allow_incomplete => code = EXIT_INCOMPLETE,
            _ => {}
        }
    }
    code
}

/// A package ecosystem as a source registry names it, wider than what this
/// registry serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceFormat {
    Npm,
    Maven,
    Raw,
    Oci,
    Pypi,
    Nuget,
    Cargo,
    Go,
    RubyGems,
    Helm,
    Other,
}

impl SourceFormat {
    pub const ALL: [SourceFormat; 11] = [
        SourceFormat::Npm,
        SourceFormat::Maven,
        SourceFormat::Raw,
        SourceFormat::Oci,
        SourceFormat::Pypi,
        SourceFormat::Nuget,
        SourceFormat::Cargo,
        SourceFormat::Go,
        SourceFormat::RubyGems,
        SourceFormat::Helm,
        SourceFormat::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            SourceFormat::Npm => "npm",
            SourceFormat::Maven => "maven",
            SourceFormat::Raw => "raw",
            SourceFormat::Oci => "oci",
            SourceFormat::Pypi => "pypi",
            SourceFormat::Nuget => "nuget",
            SourceFormat::Cargo => "cargo",
            SourceFormat::Go => "go",
            SourceFormat::RubyGems => "rubygems",
            SourceFormat::Helm => "helm",
            SourceFormat::Other => "other",
        }
    }

    /// The format this registry would store it as, if it has one.
    pub const fn target(self) -> Option<Format> {
        match self {
            SourceFormat::Npm => Some(Format::Npm),
            SourceFormat::Oci => Some(Format::Oci),
            SourceFormat::Cargo => Some(Format::Cargo),
            SourceFormat::Go => Some(Format::Go),
            SourceFormat::Pypi => Some(Format::Pypi),
            SourceFormat::Maven => Some(Format::Maven),
            SourceFormat::Nuget => Some(Format::Nuget),
            SourceFormat::Raw | SourceFormat::RubyGems | SourceFormat::Helm | SourceFormat::Other => {
                None
            }
        }
    }
}

impl FromStr for SourceFormat {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        SourceFormat::ALL
            .into_iter()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("unknown source format: {s}")))
    }
}

/// Where one planned coordinate stands in the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    Pending,
    Running,
    Copied,
    Skipped,
    Failed,
}

impl ItemStatus {
    pub const ALL: [ItemStatus; 5] = [
        ItemStatus::Pending,
        ItemStatus::Running,
        ItemStatus::Copied,
        ItemStatus::Skipped,
        ItemStatus::Failed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Pending => "pending",
            ItemStatus::Running => "running",
            ItemStatus::Copied => "copied",
            ItemStatus::Skipped => "skipped",
            ItemStatus::Failed => "failed",
        }
    }

    pub const fn terminal(self) -> bool {
        matches!(self, ItemStatus::Copied | ItemStatus::Skipped | ItemStatus::Failed)
    }
}

impl FromStr for ItemStatus {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        ItemStatus::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("unknown item status: {s}")))
    }
}

/// `*` and `?` stay inside one `/`-separated segment, `**` crosses them.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => {
                let rest = &rest[1..];
                if rest.strip_prefix(b"/").is_some_and(|after| go(after, t)) {
                    return true;
                }
                (0..=t.len()).any(|i| go(rest, &t[i..]))
            }
            Some((b'*', rest)) => {
                let mut i = 0;
                loop {
                    if go(rest, &t[i..]) {
                        return true;
                    }
                    if i == t.len() || t[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some((b'?', rest)) => t.first().is_some_and(|c| *c != b'/') && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// A source namespace folded into one lowercase OCI path segment: runs of
/// anything else become one `-`, ends trimmed. `None` when nothing is left.
pub fn oci_qualifier(namespace: &str) -> Option<String> {
    let mut out = String::with_capacity(namespace.len());
    let mut pending_sep = false;
    for c in namespace.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(c);
        } else {
            pending_sep = true;
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gap_kind_has_one_exit_class_and_the_worst_wins() {
        let blocking = GapKind::ALL.iter().filter(|k| k.class() == ExitClass::Blocking).count();
        let incomplete = GapKind::ALL.iter().filter(|k| k.class() == ExitClass::Incomplete).count();
        assert_eq!((blocking, incomplete), (6, 3));
        for kind in GapKind::ALL {
            let expected = match kind.class() {
                ExitClass::Blocking => EXIT_BLOCKING,
                ExitClass::Incomplete => EXIT_INCOMPLETE,
                ExitClass::Informational => EXIT_CLEAN,
            };
            assert_eq!(exit_code([kind], false), expected, "{kind}");
            assert_eq!(kind.as_str().parse::<GapKind>().unwrap(), kind);
        }
        assert_eq!(exit_code([GapKind::NoTarget], true), EXIT_CLEAN);
        assert_eq!(exit_code([GapKind::ListingIncomplete, GapKind::Failed], false), EXIT_BLOCKING);
        assert_eq!(exit_code([GapKind::Failed, GapKind::ListingIncomplete], true), EXIT_BLOCKING);
        assert_eq!(exit_code([], false), EXIT_CLEAN);
    }

    #[test]
    fn five_kinds_are_derived_from_items_and_never_stored() {
        let derived: Vec<_> = GapKind::ALL.into_iter().filter(|k| k.item_derived()).collect();
        assert_eq!(derived.len(), 5);
        assert!(!GapKind::TargetCollision.item_derived());
    }

    #[test]
    fn mapping_table_is_total_over_source_formats() {
        for f in SourceFormat::ALL {
            assert_eq!(f.as_str().parse::<SourceFormat>().unwrap(), f);
            let _ = f.target();
        }
        for format in Format::ALL {
            assert!(
                SourceFormat::ALL.iter().any(|s| s.target() == Some(format)),
                "{format:?} is served but no source format maps onto it"
            );
        }
        assert_eq!(SourceFormat::Raw.target(), None);
    }

    #[test]
    fn glob_matcher() {
        assert!(glob_match("npm-*/left-pad", "npm-internal/left-pad"));
        assert!(!glob_match("*", "a/b"));
        assert!(glob_match("**", "a/b/c"));
        assert!(glob_match("a/**/z", "a/b/c/z"));
        assert!(glob_match("a/**", "a/b"));
        assert!(glob_match("?at", "cat"));
        assert!(!glob_match("?at", "/at"));
        assert!(glob_match("*/@acme/*", "repo/@acme/utils"));
        assert!(!glob_match("*/@acme/*", "repo/@other/utils"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn oci_qualifier_is_normalised_to_a_valid_segment() {
        assert_eq!(oci_qualifier("docker-Hosted").as_deref(), Some("docker-hosted"));
        assert_eq!(oci_qualifier("__Proj..A__").as_deref(), Some("proj-a"));
        assert_eq!(oci_qualifier("--"), None);
        assert_eq!(oci_qualifier("library").as_deref(), Some("library"));
    }
}
