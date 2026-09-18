//! `NuGetVersion`: `Major.Minor[.Patch[.Revision]][-Release][+Metadata]`,
//! its normalized key and its precedence.
//!
//! The key is what NuGet's flat container addresses: leading zeros dropped,
//! a missing patch completed, a zero revision dropped, build metadata
//! dropped, lowercase. Two spellings with one key are one version.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NuGetVersion {
    numbers: [u64; 4],
    release: Vec<String>,
    metadata: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid NuGet version: '{0}'")]
pub struct InvalidVersion(pub String);

const MAX_LEN: usize = 256;

fn is_label(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl NuGetVersion {
    pub fn parse(raw: &str) -> Result<Self, InvalidVersion> {
        let invalid = || InvalidVersion(raw.to_string());
        if raw.is_empty() || raw.len() > MAX_LEN || raw.trim() != raw {
            return Err(invalid());
        }
        let (rest, metadata) = match raw.split_once('+') {
            Some((rest, meta)) => {
                if !meta.split('.').all(is_label) {
                    return Err(invalid());
                }
                (rest, Some(meta.to_string()))
            }
            None => (raw, None),
        };
        let (core, release) = match rest.split_once('-') {
            Some((core, release)) => {
                if !release.split('.').all(is_label) {
                    return Err(invalid());
                }
                (core, release.split('.').map(str::to_string).collect())
            }
            None => (rest, Vec::new()),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if !(2..=4).contains(&parts.len()) {
            return Err(invalid());
        }
        let mut numbers = [0u64; 4];
        for (slot, part) in numbers.iter_mut().zip(&parts) {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            *slot = part.parse().map_err(|_| invalid())?;
        }
        Ok(Self {
            numbers,
            release,
            metadata,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.release.is_empty()
    }

    /// SemVer 2.0.0 only: a dotted release label or build metadata.
    pub fn is_semver2(&self) -> bool {
        self.release.len() > 1 || self.metadata.is_some()
    }

    /// The key every uniqueness check, storage key and URL uses.
    pub fn key(&self) -> String {
        let [major, minor, patch, revision] = self.numbers;
        let mut out = format!("{major}.{minor}.{patch}");
        if revision != 0 {
            out.push_str(&format!(".{revision}"));
        }
        if self.is_prerelease() {
            out.push('-');
            out.push_str(&self.release.join("."));
        }
        out.to_ascii_lowercase()
    }

    /// The normalized form with its metadata and its case, for display.
    pub fn full(&self) -> String {
        let [major, minor, patch, revision] = self.numbers;
        let mut out = format!("{major}.{minor}.{patch}");
        if revision != 0 {
            out.push_str(&format!(".{revision}"));
        }
        if self.is_prerelease() {
            out.push('-');
            out.push_str(&self.release.join("."));
        }
        if let Some(meta) = &self.metadata {
            out.push('+');
            out.push_str(meta);
        }
        out
    }
}

impl fmt::Display for NuGetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full())
    }
}

fn compare_label(a: &str, b: &str) -> Ordering {
    let numeric = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    match (numeric(a), numeric(b)) {
        (true, true) => {
            let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            a.len().cmp(&b.len()).then_with(|| a.cmp(b))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
    }
}

/// Precedence ignores build metadata, as NuGet's does.
impl Ord for NuGetVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers
            .cmp(&other.numbers)
            .then_with(|| match (self.is_prerelease(), other.is_prerelease()) {
                (false, false) => Ordering::Equal,
                (false, true) => Ordering::Greater,
                (true, false) => Ordering::Less,
                (true, true) => {
                    for (a, b) in self.release.iter().zip(&other.release) {
                        let o = compare_label(a, b);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.release.len().cmp(&other.release.len())
                }
            })
    }
}

impl PartialOrd for NuGetVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A stored key ordered by precedence; a key that no longer parses sorts
/// first and lexically, so a corrupt row never hides the others.
pub fn compare_keys(a: &str, b: &str) -> Ordering {
    match (NuGetVersion::parse(a), NuGetVersion::parse(b)) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => Ordering::Greater,
        (Err(_), Ok(_)) => Ordering::Less,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(raw: &str) -> String {
        NuGetVersion::parse(raw).unwrap().key()
    }

    #[test]
    fn version_normalization_matches_nuget_versioning() {
        assert_eq!(key("01.002.0003"), "1.2.3", "leading zeros");
        assert_eq!(key("1.0.0.0"), "1.0.0", "a zero revision");
        assert_eq!(key("1.0.0.4"), "1.0.0.4", "a revision that is not zero");
        assert_eq!(key("1.0.0+abc.def"), "1.0.0", "build metadata");
        assert_eq!(key("1.0.0-Beta.1"), "1.0.0-beta.1", "case");
        assert_eq!(key("1.0"), "1.0.0", "the patch is completed");
        for spelling in ["1.0", "1.0.0.0", "1.0.0+abc", "01.0.0", "1.0.0"] {
            assert_eq!(key(spelling), "1.0.0", "{spelling}");
        }
        assert_eq!(NuGetVersion::parse("1.0.0-RC+Git").unwrap().full(), "1.0.0-RC+Git");
    }

    #[test]
    fn malformed_versions_are_refused() {
        for bad in [
            "", "1", "1.2.3.4.5", "a.b", "1.0.0-", "1.0.0-a..b", "1.0.0+", "1.0.0-a_b", " 1.0",
            "1.0/..", "1.-1.0",
        ] {
            assert!(NuGetVersion::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn precedence_follows_nuget() {
        let v = |s| NuGetVersion::parse(s).unwrap();
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.9"));
        assert!(v("1.0.0-alpha") < v("1.0.0-Beta"), "labels compare case-insensitively");
        assert!(v("1.0.0-1") < v("1.0.0-a"));
        assert!(v("1.0.0.1") > v("1.0.0"));
        assert!(v("1.0.0-rc") < v("1.0.0-rc.1"));
        assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal);
        assert!(v("1.0.0-beta.2").is_semver2());
        assert!(v("1.0.0+meta").is_semver2());
        assert!(!v("1.0.0-beta").is_semver2());
        assert_eq!(compare_keys("1.10.0", "1.9.0"), Ordering::Greater);
    }
}
