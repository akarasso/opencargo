//! What a server name and a server version may be.
//!
//! A name is `<reverse-dns namespace>/<name>`: every allow rule, approval and
//! generated client config is keyed on it, so a record whose name does not
//! have that shape is refused rather than stored.

use crate::domain::{compile_pattern, DomainError, FormatRules, Pattern};

pub struct McpRules;

const MAX_NAME: usize = 200;
const MAX_VERSION: usize = 255;

fn namespace_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

fn leaf_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')
}

fn invalid(what: &str, value: &str) -> DomainError {
    DomainError::InvalidName(format!("invalid MCP server {what}: '{value}'"))
}

impl FormatRules for McpRules {
    /// A server name is already its own canonical spelling: the namespace and
    /// the leaf are matched byte for byte, so the three keys are the name.
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
        let Some((namespace, leaf)) = name.split_once('/') else {
            return Err(invalid("name", name));
        };
        let ok = name.len() <= MAX_NAME
            && !namespace.is_empty()
            && !leaf.is_empty()
            && namespace.bytes().all(namespace_byte)
            && leaf.bytes().all(leaf_byte)
            && !namespace.starts_with(['.', '-'])
            && !namespace.ends_with(['.', '-'])
            && !namespace.contains("..")
            && leaf != "."
            && leaf != "..";
        if ok {
            Ok(())
        } else {
            Err(invalid("name", name))
        }
    }

    fn normalize(&self, name: &str) -> String {
        name.to_string()
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        let ok = !version.is_empty()
            && version.len() <= MAX_VERSION
            && version != "latest"
            && !version.contains('/')
            && version.chars().all(|c| !c.is_control() && !c.is_whitespace());
        if ok {
            Ok(())
        } else {
            Err(invalid("version", version))
        }
    }

    fn normalize_version(&self, version: &str) -> String {
        version.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_a_reverse_dns_namespace_and_a_leaf() {
        for good in ["io.github.acme/server", "com.stripe/mcp", "ai.aard/aard", "a/b_c.d-e"] {
            assert!(McpRules.validate(good).is_ok(), "{good}");
        }
        for bad in [
            "server",
            "/server",
            "io.github/",
            "io.github/a/b",
            "io..github/a",
            ".io/a",
            "io/..",
            "io github/a",
            "io.github/a%2Fb",
        ] {
            assert!(McpRules.validate(bad).is_err(), "{bad}");
        }
        assert!(McpRules.validate(&format!("a/{}", "x".repeat(MAX_NAME))).is_err());
    }

    #[test]
    fn a_version_is_one_opaque_segment_and_never_latest() {
        for good in ["1.0.0", "0.2.0-beta.1", "2025.09.01"] {
            assert!(McpRules.validate_version(good).is_ok(), "{good}");
        }
        for bad in ["", "latest", "1.0/2", "1 0", "1.0\n"] {
            assert!(McpRules.validate_version(bad).is_err(), "{bad:?}");
        }
    }
}
