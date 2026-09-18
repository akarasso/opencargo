use crate::domain::{DomainError, FormatRules};

use super::version::NuGetVersion;

const MAX_ID_LEN: usize = 100;

/// NuGet's id grammar, ASCII only: word runs joined by single `.`, `-` or
/// `_`, at most a hundred characters.
fn is_valid_id(id: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
        return false;
    }
    if !word(bytes[0]) || !word(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut previous_separator = false;
    for &b in bytes {
        let separator = matches!(b, b'.' | b'-');
        if !(word(b) || separator) || (separator && previous_separator) {
            return false;
        }
        previous_separator = separator;
    }
    true
}

pub struct NugetRules;

impl FormatRules for NugetRules {
    fn validate(&self, name: &str) -> Result<(), DomainError> {
        if is_valid_id(name) {
            Ok(())
        } else {
            Err(DomainError::InvalidName(format!(
                "invalid nuget package name: '{name}'"
            )))
        }
    }

    fn normalize(&self, name: &str) -> String {
        name.to_ascii_lowercase()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        NuGetVersion::parse(version)
            .map(|_| ())
            .map_err(|e| DomainError::InvalidName(e.to_string()))
    }

    /// An unparseable version is its own lowercase: `validate_version`
    /// refuses it before any key is built.
    fn normalize_version(&self, version: &str) -> String {
        NuGetVersion::parse(version)
            .map(|v| v.key())
            .unwrap_or_else(|_| version.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_nuget_grammar() {
        for good in ["Newtonsoft.Json", "a", "My_Lib-2", "x.y.z"] {
            assert!(is_valid_id(good), "{good}");
        }
        let long = "a".repeat(101);
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "a/b",
            "a b",
            "../x",
            "é",
            long.as_str(),
        ] {
            assert!(!is_valid_id(bad), "{bad:?}");
        }
    }
}
