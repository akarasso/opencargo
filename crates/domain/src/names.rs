//! Package names, versions and tags come straight from a URL or a request
//! body and end up interpolated into storage paths (`npm/{repo}/{name}/...`,
//! `cargo/...`, `go/...`). The storage backend blocks escapes from its root,
//! but without validation a name containing `/` still creates an arbitrary
//! directory tree, pollutes the store, and produces unresolvable packages.
//! These are the per-ecosystem naming rules, enforced at the publish
//! boundary.

use super::DomainError;

/// Unscoped npm name part (also used for the scope and the name of a scoped
/// package): lowercase `[a-z0-9._-]+`, must not start with `.` or `_`.
fn is_valid_npm_name_part(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && !part.starts_with('_')
        && part
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// A Go module path segment: `[a-zA-Z0-9._~-]+`, and neither `.` nor `..`.
fn is_valid_go_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|b| {
            matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'~' | b'-')
        })
}

/// An OCI repository-name segment per the distribution spec:
/// `[a-z0-9]+(?:[._-][a-z0-9]+)*` — alphanumeric runs joined by single
/// separators, never leading/trailing or doubled.
fn is_valid_oci_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let is_alnum = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9');
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut prev_was_separator = false;
    for &b in bytes {
        if is_alnum(b) {
            prev_was_separator = false;
        } else if matches!(b, b'.' | b'_' | b'-') {
            if prev_was_separator {
                return false;
            }
            prev_was_separator = true;
        } else {
            return false;
        }
    }
    true
}

/// Validate a package name for the given ecosystem (`npm`, `cargo`, `go`,
/// `oci`). Called at the top of every publish handler so hostile names (path
/// separators, traversal sequences, forbidden characters) are refused before
/// anything is stored.
pub fn validate_package_name(format: &str, name: &str) -> Result<(), DomainError> {
    let invalid = || DomainError::InvalidName(format!("invalid {format} package name: '{name}'"));

    match format {
        "npm" => {
            // Unscoped: [a-z0-9._-]+ ; scoped: @scope/name with exactly one
            // '/', both parts following the unscoped rule. Max 214 chars total
            // (npm registry limit), no leading '.' or '_'.
            if name.len() > 214 {
                return Err(invalid());
            }
            match name.strip_prefix('@') {
                Some(rest) => {
                    let (scope, pkg) = rest.split_once('/').ok_or_else(invalid)?;
                    if pkg.contains('/')
                        || !is_valid_npm_name_part(scope)
                        || !is_valid_npm_name_part(pkg)
                    {
                        return Err(invalid());
                    }
                }
                None => {
                    if !is_valid_npm_name_part(name) {
                        return Err(invalid());
                    }
                }
            }
        }
        "cargo" => {
            // [a-zA-Z][a-zA-Z0-9_-]*, max 64 chars (crates.io rules).
            if name.is_empty() || name.len() > 64 {
                return Err(invalid());
            }
            let mut bytes = name.bytes();
            if !bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) {
                return Err(invalid());
            }
            if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(invalid());
            }
        }
        "go" => {
            // Module path: non-empty segments of [a-zA-Z0-9._~-] separated by
            // '/', no '.'/'..' segments. Capped at 255 chars to keep the
            // user-controlled storage key bounded.
            if name.is_empty() || name.len() > 255 {
                return Err(invalid());
            }
            if !name.split('/').all(is_valid_go_segment) {
                return Err(invalid());
            }
        }
        "oci" => {
            // Distribution-spec repository name: '/'-separated segments, each
            // [a-z0-9]+(?:[._-][a-z0-9]+)*. Capped at 255 chars (spec limit).
            if name.is_empty() || name.len() > 255 {
                return Err(invalid());
            }
            if !name.split('/').all(is_valid_oci_segment) {
                return Err(invalid());
            }
        }
        other => {
            return Err(DomainError::InvalidName(format!(
                "unsupported package format: {other}"
            )));
        }
    }
    Ok(())
}

/// One npm name part on a read: any legacy name npm still serves (uppercase,
/// `~'!()*`) passes, only what would escape a path or break a URL is refused.
fn is_safe_npm_read_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part.bytes().all(|b| {
            b.is_ascii_graphic() && !matches!(b, b'/' | b'\\' | b'?' | b'#' | b'%')
        })
}

/// The read-side npm rule: publish enforces npm's naming policy through
/// [`validate_package_name`], reads only need the name to be one safe path
/// segment (or `@scope/name`), so packages published before the lowercase
/// rule (`JSONStream`, `Base64`) keep resolving through a proxy.
pub fn validate_npm_read_name(name: &str) -> Result<(), DomainError> {
    let invalid = || DomainError::InvalidName(format!("invalid npm package name: '{name}'"));
    if name.len() > 214 {
        return Err(invalid());
    }
    let safe = match name.strip_prefix('@') {
        Some(rest) => rest
            .split_once('/')
            .is_some_and(|(scope, pkg)| is_safe_npm_read_part(scope) && is_safe_npm_read_part(pkg)),
        None => is_safe_npm_read_part(name),
    };
    if safe {
        Ok(())
    } else {
        Err(invalid())
    }
}

/// Validate a version string: `[a-zA-Z0-9.+-]+`, at most 128 chars. Covers
/// semver (npm/cargo) and Go pseudo-versions; blocks path separators and
/// traversal sequences in storage paths like `{name}-{version}.crate`.
pub fn validate_version(version: &str) -> Result<(), DomainError> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    {
        return Err(DomainError::InvalidName(format!(
            "invalid version: '{version}'"
        )));
    }
    Ok(())
}

/// Validate an OCI tag per the distribution spec:
/// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. Kept separate from
/// [`validate_version`] because legal docker tags may contain `_` and
/// uppercase, which the generic version rule rejects.
pub fn validate_oci_tag(tag: &str) -> Result<(), DomainError> {
    let invalid = || DomainError::InvalidName(format!("invalid OCI tag: '{tag}'"));
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err(invalid());
    }
    if !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_') {
        return Err(invalid());
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::{
        validate_npm_read_name, validate_oci_tag, validate_package_name, validate_version,
    };

    #[test]
    fn npm_read_names_accept_legacy_case_and_refuse_path_escapes() {
        for ok in [
            "JSONStream",
            "Base64",
            "@Scope/CSSselect",
            "react",
            "@scope/pkg",
            "lodash.merge",
            "weird~name!(1)*",
        ] {
            assert!(validate_npm_read_name(ok).is_ok(), "{ok} should be readable");
            assert!(
                validate_package_name("npm", ok).is_err() || ok.to_lowercase() == ok,
                "{ok}: publish stays strict"
            );
        }
        for bad in [
            "",
            "..",
            "a/b",
            "../evil",
            "@scope",
            "@scope/a/b",
            "@/pkg",
            "@scope/",
            "@scope/..",
            "a b",
            "a\\b",
            "a?b",
            "a#b",
            "a%2fb",
            "\u{1}",
            &"x".repeat(215),
        ] {
            assert!(validate_npm_read_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn npm_names() {
        for ok in [
            "react",
            "lodash.merge",
            "my-pkg_2",
            "@scope/pkg",
            "@my.scope/my-pkg_x",
        ] {
            assert!(validate_package_name("npm", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "React",          // uppercase
            ".dotfirst",      // leading dot
            "_underfirst",    // leading underscore
            "a/b",            // unscoped with slash -> arbitrary storage tree
            "../evil",        // traversal
            "@scope",         // scoped without name
            "@scope/a/b",     // two slashes
            "@/pkg",          // empty scope
            "@scope/",        // empty name
            "@scope/.dot",    // leading dot in name part
            "@Scope/pkg",     // uppercase scope
            "a b",            // space
            &"x".repeat(215), // too long
        ] {
            assert!(validate_package_name("npm", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn cargo_names() {
        for ok in ["serde", "X9", "a-b_c123", &"a".repeat(64)] {
            assert!(validate_package_name("cargo", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "1abc",           // must start with a letter
            "-abc",
            "_abc",
            "a.b",            // dots not allowed
            "a/b",
            "../evil",
            &"a".repeat(65),  // too long
        ] {
            assert!(validate_package_name("cargo", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn go_names() {
        for ok in [
            "mymodule",
            "github.com/org/repo",
            "golang.org/x/tools",
            "example.com/Org_1/~repo-v2",
        ] {
            assert!(validate_package_name("go", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "a//b",           // empty segment
            "/a/b",           // leading slash -> empty segment
            "a/b/",           // trailing slash -> empty segment
            "a/../b",         // traversal segment
            "../evil",
            "./a",
            "a b/c",          // space
            "a!b",            // forbidden char
            &format!("a/{}", "b".repeat(255)), // too long
        ] {
            assert!(validate_package_name("go", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn oci_names() {
        for ok in ["myapp", "my-app.v2", "a/b", "team1/backend_api"] {
            assert!(validate_package_name("oci", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "MyApp",          // uppercase
            "-lead",          // leading separator
            "trail-",         // trailing separator
            "a..b",           // doubled separator
            "a__b",           // doubled separator
            "a//b",           // empty segment
            "../evil",
            "a b",
        ] {
            assert!(validate_package_name("oci", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn unknown_format_rejected() {
        assert!(validate_package_name("pypi", "anything").is_err());
    }

    #[test]
    fn versions() {
        for ok in ["1.0.0", "v1.2.3", "1.0.0-beta.1+build.5", "0.0.0-20230101120000-abcdef123456"] {
            assert!(validate_version(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", "1.0.0/evil", "../1.0.0", "1.0 .0", "1_0", &"1".repeat(129)] {
            assert!(validate_version(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn oci_tags() {
        for ok in ["latest", "v1.2.3", "main_build-42", "_internal", "V2"] {
            assert!(validate_oci_tag(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", ".hidden", "-lead", "sha256:abc", "a/b", &"t".repeat(129)] {
            assert!(validate_oci_tag(bad).is_err(), "{bad} should be rejected");
        }
    }
}
