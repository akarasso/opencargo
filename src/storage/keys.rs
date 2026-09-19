//! The key rules every backend enforces identically, before any I/O.

use super::StorageError;

/// In-flight writer scratch.
pub const SCRATCH: &str = "_scratch";
/// Backend-owned objects: the health probe, the self-check tree.
pub const BACKEND: &str = "_backend";

/// Longest key a backend with no prefix of its own accepts.
pub const MAX_KEY_BYTES: usize = 1024;

/// What a backend can key: the whole key, and any one segment of it. A
/// backend that bounds no segment reports the key's own budget for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyBudget {
    pub key: usize,
    pub segment: usize,
}

impl KeyBudget {
    pub const UNSEGMENTED: KeyBudget = KeyBudget {
        key: MAX_KEY_BYTES,
        segment: MAX_KEY_BYTES,
    };
}

fn invalid(why: &str) -> StorageError {
    StorageError::InvalidPath(why.to_string())
}

/// For a backend whose segments are names of something: no segment of
/// `key` over `max` bytes.
pub fn validate_segments(key: &str, max: usize) -> Result<(), StorageError> {
    if key.split('/').any(|s| s.len() > max) {
        return Err(invalid("storage key segment too long"));
    }
    Ok(())
}

/// A key a trait call may name. `budget` is what the backend leaves once its
/// own prefix is counted.
pub fn validate(key: &str, budget: usize) -> Result<(), StorageError> {
    if key.is_empty() {
        return Err(invalid("empty storage key"));
    }
    validate_prefix(key, budget)
}

/// A key or a listing prefix; the empty prefix lists everything.
pub fn validate_prefix(prefix: &str, budget: usize) -> Result<(), StorageError> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.contains("..") {
        return Err(invalid("path must not contain '..'"));
    }
    if prefix.len() > budget {
        return Err(invalid("storage key too long"));
    }
    if prefix
        .bytes()
        .any(|b| b == b'\\' || b == 0 || b.is_ascii_control())
    {
        return Err(invalid("invalid character in storage key"));
    }
    let mut segments = prefix.split('/');
    let first = segments.next().unwrap_or_default();
    if first == SCRATCH || first == BACKEND {
        return Err(invalid("reserved storage segment"));
    }
    if std::iter::once(first)
        .chain(segments)
        .any(|s| s.is_empty() || s == ".")
    {
        return Err(invalid("invalid storage key segment"));
    }
    Ok(())
}

/// Whether `key` is `prefix` itself or lies under it, segment-wise.
pub fn under(key: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_before_any_io() {
        for good in ["a", "npm/r/p/p-1.0.0.tgz", "_proxy_cache/p/k/ab/abc", "%2e%2e/x"] {
            assert!(validate(good, MAX_KEY_BYTES).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "a/",
            "a/./b",
            "../x",
            "a/..",
            "foo..bar",
            "a\\b",
            "_scratch/x",
            "_backend",
        ] {
            assert!(
                matches!(validate(bad, MAX_KEY_BYTES), Err(StorageError::InvalidPath(_))),
                "{bad:?}"
            );
        }
        assert!(validate("abcd", 3).is_err(), "the backend's budget counts");
        assert!(validate_prefix("", 3).is_ok());
        assert!(validate_segments("ab/abc/ab", 3).is_ok());
        assert!(matches!(
            validate_segments("ab/abcd/ab", 3),
            Err(StorageError::InvalidPath(_))
        ));
    }

    #[test]
    fn under_is_segment_wise() {
        assert!(under("a/b", "a/b"));
        assert!(under("a/b/c", "a/b"));
        assert!(!under("a/bc", "a/b"));
        assert!(under("anything", ""));
    }
}
