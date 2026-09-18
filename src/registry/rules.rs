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

/// Every format has its rules; `Option` keeps the table total.
pub fn rules(format: Format) -> Option<&'static dyn FormatRules> {
    match format {
        Format::Npm => Some(&NpmRules),
        Format::Cargo => Some(&CargoRules),
        Format::Go => Some(&GoRules),
        Format::Oci => Some(&OciRules),
        Format::Pypi => Some(&super::pypi::names::PypiRules),
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
    fn every_format_has_its_rules() {
        for format in Format::ALL {
            assert!(rules_of(format).is_ok(), "{format:?}");
        }
    }
}
