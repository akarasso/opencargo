//! The table `Format -> FormatRules`. npm, cargo, go and OCI reproduce the
//! rules they had before the table existed: versions are compared as
//! published, and only cargo folds the case of a name.

use crate::domain::{
    validate_oci_tag, validate_package_name, validate_version, DomainError, Format, FormatRules,
};

pub struct NpmRules;
pub struct CargoRules;
pub struct GoRules;
pub struct OciRules;
pub struct MavenRules;

impl FormatRules for NpmRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        validate_package_name("npm", name)
    }

    fn normalize(&self, name: &str) -> String {
        name.to_string()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        validate_version(version)
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

impl FormatRules for CargoRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        validate_package_name("cargo", name)
    }

    fn normalize(&self, name: &str) -> String {
        name.to_ascii_lowercase()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        validate_version(version)
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

impl FormatRules for GoRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        validate_package_name("go", name)
    }

    fn normalize(&self, name: &str) -> String {
        name.to_string()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        validate_version(version)
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

/// An OCI "version" is a tag.
impl FormatRules for OciRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        validate_package_name("oci", name)
    }

    fn normalize(&self, name: &str) -> String {
        name.to_string()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        validate_oci_tag(version)
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

/// A name is `groupId:artifactId`, both case-sensitive; a version is one
/// path segment, compared as published.
impl FormatRules for MavenRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        let invalid = || DomainError::InvalidName(format!("invalid maven coordinates: '{name}'"));
        let (group, artifact) = name.split_once(':').ok_or_else(invalid)?;
        let segment = |s: &str| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        if name.len() > 255 || !group.split('.').all(segment) || !segment(artifact) {
            return Err(invalid());
        }
        Ok(())
    }

    fn normalize(&self, name: &str) -> String {
        name.to_string()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        let ok = !version.is_empty()
            && version.len() <= 128
            && !version.starts_with('.')
            && !version.contains("..")
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
        if ok {
            Ok(())
        } else {
            Err(DomainError::InvalidName(format!("invalid version: '{version}'")))
        }
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

/// Every format has its rules; `Option` keeps the table total.
pub fn rules(format: Format) -> Option<&'static dyn FormatRules> {
    match format {
        Format::Npm => Some(&NpmRules),
        Format::Cargo => Some(&CargoRules),
        Format::Go => Some(&GoRules),
        Format::Oci => Some(&OciRules),
        Format::Pypi => Some(&super::pypi::names::PypiRules),
        Format::Maven => Some(&MavenRules),
        Format::Nuget => Some(&super::nuget::rules::NugetRules),
        Format::Mcp => Some(&super::mcp::rules::McpRules),
    }
}

pub fn rules_of(format: Format) -> Result<&'static dyn FormatRules, DomainError> {
    rules(format).ok_or_else(|| {
        DomainError::InvalidName(format!("unsupported package format: {}", format.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(rules: &dyn FormatRules, a: &str, b: &str) -> bool {
        rules.normalize(a) == rules.normalize(b)
    }

    fn same_version(rules: &dyn FormatRules, a: &str, b: &str) -> bool {
        rules.normalize_version(a) == rules.normalize_version(b)
    }

    #[test]
    fn npm_uniqueness_verdicts_are_unchanged() {
        let r = rules_of(Format::Npm).unwrap();
        assert!(!same(r, "left-pad", "Left-Pad"));
        assert!(!same_version(r, "1.0.0", "1.0.0+build"));
        assert!(!same_version(r, "1.0.0", "v1.0.0"));
        assert!(r.admit("Bad Name").is_err());
        assert!(r.validate_version("1.0.0/..").is_err());
    }

    #[test]
    fn cargo_uniqueness_verdicts_are_unchanged() {
        let r = rules_of(Format::Cargo).unwrap();
        assert!(same(r, "Serde", "serde"), "cargo names are case-insensitive");
        assert!(!same(r, "serde-json", "serde_json"));
        assert!(!same_version(r, "1.0.0", "1.0.0+meta"));
        assert_eq!(r.admit("MyCrate").unwrap(), "mycrate");
    }

    #[test]
    fn go_uniqueness_verdicts_are_unchanged() {
        let r = rules_of(Format::Go).unwrap();
        assert!(!same(r, "github.com/A/b", "github.com/a/b"));
        assert!(!same_version(r, "v1.0.0", "v1.0.0+incompatible"));
        assert!(r.validate("github.com/a/../b").is_err());
    }

    #[test]
    fn oci_uniqueness_verdicts_are_unchanged() {
        let r = rules_of(Format::Oci).unwrap();
        assert!(!same_version(r, "Latest", "latest"));
        assert!(r.validate_version("v1_rc").is_ok(), "a tag, not a semver");
        assert!(r.validate("Upper/Case").is_err());
    }

    #[test]
    fn nuget_spellings_of_one_version_are_one_key() {
        let r = rules_of(Format::Nuget).unwrap();
        assert!(same(r, "Newtonsoft.Json", "newtonsoft.json"));
        for spelling in ["1.0", "1.0.0.0", "1.0.0+abc", "01.0.0"] {
            assert!(same_version(r, spelling, "1.0.0"), "{spelling}");
        }
        assert!(!same_version(r, "1.0.0-beta", "1.0.0"));
        assert!(r.validate_version("1.0.0/..").is_err());
        assert!(r.admit("../x").is_err());
    }

    #[test]
    fn maven_coordinates_are_group_colon_artifact() {
        let r = rules_of(Format::Maven).unwrap();
        assert!(r.validate("org.example:lib-core").is_ok());
        assert!(!same(r, "org.Example:lib", "org.example:lib"));
        for bad in ["lib", "org..x:lib", ":lib", "org.x:", "org/x:lib", "org.x:../lib", ".org:lib"] {
            assert!(r.validate(bad).is_err(), "{bad}");
        }
        assert!(r.validate_version("1.0-SNAPSHOT").is_ok());
        assert!(r.validate_version("1.0_beta+2").is_ok());
        for bad in ["", "1/0", "..", ".1", "1 0"] {
            assert!(r.validate_version(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_format_has_its_rules() {
        for format in Format::ALL {
            assert!(rules_of(format).is_ok(), "{format:?}");
        }
    }
}
