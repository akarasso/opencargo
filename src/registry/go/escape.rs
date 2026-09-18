use crate::error::{AppError, AppResult};

/// GOPROXY path escaping per `golang.org/x/mod/module`: `A` <-> `!a`, `!` <-> `!!`.
pub fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            'A'..='Z' => {
                out.push('!');
                out.push(c.to_ascii_lowercase());
            }
            '!' => out.push_str("!!"),
            _ => out.push(c),
        }
    }
    out
}

pub fn unescape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('!', Some('!')) => {
                chars.next();
                out.push('!');
            }
            ('!', Some(next)) if next.is_ascii_lowercase() => {
                chars.next();
                out.push(next.to_ascii_uppercase());
            }
            _ => out.push(c),
        }
    }
    out
}

/// Every `!` is followed by `[a-z]` or another `!`.
fn escapes_well_formed(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'!' {
            match bytes.get(i + 1) {
                Some(b'a'..=b'z') | Some(b'!') => i += 1,
                _ => return false,
            }
        }
        i += 1;
    }
    true
}

/// The `go` name rule on the module with its escapes stripped, plus every
/// `!` followed by `[a-z]` or `!`.
pub fn validate_escaped_module(module: &str) -> AppResult<()> {
    let invalid = || AppError::BadRequest(format!("invalid go module path: '{module}'"));
    if !escapes_well_formed(module) {
        return Err(invalid());
    }
    crate::registry::rules::rules_of(crate::domain::Format::Go)?.validate(&module.replace('!', "")).map_err(|_| invalid())
}

/// The version is case-encoded like the module (`v1.0.0-RC1` arrives as
/// `v1.0.0-!r!c1`): the generic version rule applies to the unescaped form.
pub fn validate_escaped_version(version: &str) -> AppResult<()> {
    let invalid = || AppError::BadRequest(format!("invalid version: '{version}'"));
    if !escapes_well_formed(version) {
        return Err(invalid());
    }
    crate::registry::rules::rules_of(crate::domain::Format::Go)?.validate_version(&unescape(version)).map_err(|_| invalid())
}

/// `v` + semver core with optional pre-release/build; pseudo-versions
/// qualify, branch names and `v1`/`v1.2` queries do not.
pub fn is_canonical_version(version: &str) -> bool {
    version
        .strip_prefix('v')
        .is_some_and(|v| semver::Version::parse(v).is_ok())
}

#[cfg(test)]
mod tests {
    use super::{
        escape, is_canonical_version, unescape, validate_escaped_module,
        validate_escaped_version,
    };

    #[test]
    fn roundtrip_and_validation() {
        for (raw, escaped) in [
            ("github.com/BurntSushi/toml", "github.com/!burnt!sushi/toml"),
            ("golang.org/x/tools", "golang.org/x/tools"),
            ("example.com/A!B", "example.com/!a!!!b"),
        ] {
            assert_eq!(escape(raw), escaped);
            assert_eq!(unescape(escaped), raw);
        }

        for ok in [
            "github.com/!burnt!sushi/toml",
            "example.com/org/repo",
            "example.com/a!!b",
            "mymodule",
        ] {
            assert!(validate_escaped_module(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "example.com/!",
            "example.com/!Bad",
            "example.com/!1",
            "example.com//repo",
            "../evil",
            "a b",
        ] {
            assert!(
                validate_escaped_module(bad).is_err(),
                "{bad} should be rejected"
            );
        }

        for ok in ["v1.0.0", "v1.0.0-!r!c1", "v0.0.0-20230101120000-abcdef123456"] {
            assert!(validate_escaped_version(ok).is_ok(), "{ok}");
        }
        for bad in ["", "v1.0.0-!R", "v1!", "1.0.0/evil", "v1.0.0-!1"] {
            assert!(validate_escaped_version(bad).is_err(), "{bad}");
        }

        for canonical in [
            "v1.0.0",
            "v1.2.3-rc.1",
            "v2.0.0+incompatible",
            "v0.0.0-20230101120000-abcdef123456",
        ] {
            assert!(is_canonical_version(canonical), "{canonical}");
        }
        for query in ["master", "latest", "v1", "v1.2", "1.0.0", "v01.0.0"] {
            assert!(!is_canonical_version(query), "{query}");
        }
    }
}
