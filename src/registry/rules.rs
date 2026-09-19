//! The table `Format -> FormatRules`. npm, cargo, go and OCI reproduce the
//! rules they had before the table existed: versions are compared as
//! published, and only cargo folds the case of a name.

use crate::domain::{
    compile_pattern, validate_oci_tag, validate_package_name, validate_version, DomainError,
    Format, FormatRules, Pattern,
};

/// The common half of every format's routing canonicalization: ASCII case
/// folded away. What each format adds to it is written in its own impl.
fn ascii_fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub struct NpmRules;
pub struct CargoRules;
pub struct GoRules;
pub struct OciRules;
pub struct MavenRules;

impl FormatRules for NpmRules {
    /// The npm store serves a name byte for byte (`NameMatch::Exact`), so
    /// identity is the spelling itself.
    fn ident_key(&self, name: &str) -> String {
        name.to_string()
    }

    /// ASCII case only: npmjs refuses two names differing in case alone, so
    /// folding it cannot merge two packages that both exist upstream.
    fn match_key(&self, name: &str) -> String {
        ascii_fold(name)
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, ascii_fold)
    }

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
    /// The cargo store matches case-insensitively, so a case class is one row.
    fn ident_key(&self, name: &str) -> String {
        ascii_fold(name)
    }

    /// Plus `_` folded to `-`: crates.io refuses the pair `a_b`/`a-b`, so the
    /// extra fold cannot close a name that really exists upstream.
    fn match_key(&self, name: &str) -> String {
        ascii_fold(name).replace('_', "-")
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, |p| ascii_fold(p).replace('_', "-"))
    }

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
    /// Routing sees the module path already unescaped, and the Go store holds
    /// `github.com/acme/tool` and `github.com/Acme/tool` as two rows.
    fn ident_key(&self, name: &str) -> String {
        name.to_string()
    }

    fn match_key(&self, name: &str) -> String {
        ascii_fold(name)
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, ascii_fold)
    }

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
    /// The only spelling that reaches an OCI route is already lowercase, so
    /// the two keys coincide and neither coarsens anything.
    fn ident_key(&self, name: &str) -> String {
        name.to_string()
    }

    fn match_key(&self, name: &str) -> String {
        name.to_string()
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, str::to_string)
    }

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
    fn ident_key(&self, name: &str) -> String {
        name.to_string()
    }

    fn match_key(&self, name: &str) -> String {
        ascii_fold(name)
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, ascii_fold)
    }

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

    /// Every spelling that reaches the read path of its format: the space the
    /// relation below has to hold over, not the narrower publish space.
    fn read_spellings(format: Format) -> &'static [&'static str] {
        match format {
            Format::Npm => &[
                "@acme/foo",
                "@ACME/foo",
                "@Acme/Foo",
                "left-pad",
                "JSONStream",
                "jsonstream",
                "weird~name!(1)*",
            ],
            Format::Cargo => &["acme_lib", "Acme_Lib", "acme-lib", "ACME-LIB", "serde"],
            Format::Go => &[
                "github.com/acme/tool",
                "github.com/Acme/tool",
                "github.com/ACME/TOOL",
                "gopkg.in/yaml.v3",
            ],
            Format::Oci => &["acme/app", "acme/app-base", "library/nginx"],
            Format::Maven => &["com.acme:lib", "com.Acme:lib", "COM.ACME:LIB", "org.x:y"],
            Format::Pypi => &["acme-lib", "Acme.Lib", "ACME__LIB", "acme_lib", "requests"],
            Format::Nuget => &["Acme.Lib", "acme.lib", "ACME.LIB", "Newtonsoft.Json"],
        }
    }

    /// I7: `ker(ident_key) ⊆ ker(match_key)`. Two spellings the store serves
    /// as one row always share a pattern key, so coarsening can only ever
    /// refuse more — never let a spelling through.
    #[test]
    fn the_match_key_coarsens_store_identity_in_every_format() {
        for format in Format::ALL {
            let r = rules_of(format).unwrap();
            let names = read_spellings(format);
            for a in names {
                for b in names {
                    if r.ident_key(a) == r.ident_key(b) {
                        assert_eq!(
                            r.match_key(a),
                            r.match_key(b),
                            "{format:?}: {a} and {b} are one row but two pattern keys"
                        );
                    }
                }
            }
        }
    }

    /// The seam of D0quater: a pattern is canonicalized by the same function
    /// as the key, so a star substituted with any admissible spelling still
    /// matches.
    #[test]
    fn a_pattern_canonicalizes_like_the_key_it_is_compared_to() {
        let cases: &[(Format, &str, &[&str], &[&str])] = &[
            (Format::Npm, "@acme/*", &["@acme/foo", "@ACME/foo"], &["@acmex/foo"]),
            (Format::Cargo, "acme_*", &["acme-lib", "Acme_Lib"], &["acmelib"]),
            (
                Format::Go,
                "github.com/acme/*",
                &["github.com/acme/tool", "github.com/Acme/tool"],
                &["github.com/acmex/tool"],
            ),
            (Format::Oci, "acme/*", &["acme/app"], &["acmefoo/app"]),
            (Format::Maven, "com.acme:*", &["com.acme:lib", "com.Acme:lib"], &["com.acmex:lib"]),
            (Format::Pypi, "Acme_*", &["acme-lib", "ACME.LIB"], &["acmelib"]),
            (Format::Nuget, "Acme.*", &["acme.lib", "ACME.LIB"], &["acmex.lib"]),
        ];
        for (format, pattern, inside, outside) in cases {
            let r = rules_of(*format).unwrap();
            let compiled = r.canonical_pattern(pattern).unwrap();
            for name in *inside {
                assert!(compiled.matches(&r.match_key(name)), "{format:?}: {name}");
            }
            for name in *outside {
                assert!(!compiled.matches(&r.match_key(name)), "{format:?}: {name}");
            }
        }
    }

    /// D4's alphabet, on the one format whose read path admits `*` and `!` as
    /// name characters: a key is compared literally, so no name escapes
    /// through its own metacharacters.
    #[test]
    fn a_name_never_escapes_through_its_own_metacharacters() {
        let r = rules_of(Format::Npm).unwrap();
        let legacy = "weird~name!(1)*";
        assert!(crate::domain::validate_npm_read_name(legacy).is_ok());
        let compiled = r.canonical_pattern("weird*").unwrap();
        assert!(compiled.matches(&r.match_key(legacy)), "filtered, not interpreted");
        assert!(r.canonical_pattern("weird\\*name").is_err(), "no escape exists");
        assert!(r.canonical_pattern("").is_err());
    }
}
