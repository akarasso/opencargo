//! How often one account may publish: the shape of a limit, where a limit
//! comes from, and which one applies to a publish. The counting itself needs
//! a clock and a map, so it lives outside.

use std::collections::HashMap;

use crate::kinds::Format;

/// A limit is finite by construction: `max` is at least one and the window is
/// a bounded number of seconds, so a configured limit can be raised but never
/// turned into no limit at all.
pub const MAX_PER_WINDOW: u32 = 1_000_000;
pub const MAX_WINDOW_SECS: u64 = 86_400;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LimitError {
    #[error("a publish limit must allow at least one publish per window")]
    Empty,

    #[error("a publish limit may not exceed {MAX_PER_WINDOW} per window")]
    TooLarge,

    #[error("a publish window must be between 1s and {MAX_WINDOW_SECS}s")]
    Window,
}

/// `max` publishes per `window_secs`, per account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishLimit {
    max: u32,
    window_secs: u64,
}

impl PublishLimit {
    pub fn new(max: u32, window_secs: u64) -> Result<Self, LimitError> {
        if max == 0 {
            return Err(LimitError::Empty);
        }
        if max > MAX_PER_WINDOW {
            return Err(LimitError::TooLarge);
        }
        if window_secs == 0 || window_secs > MAX_WINDOW_SECS {
            return Err(LimitError::Window);
        }
        Ok(Self { max, window_secs })
    }

    pub fn max(self) -> u32 {
        self.max
    }

    pub fn window_secs(self) -> u64 {
        self.window_secs
    }
}

/// Which entry a limit was read from. It is part of the counter's key, so a
/// repository's own allowance is spent separately from its format's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitScope {
    Repository(String),
    Format(Format),
    Every,
}

impl LimitScope {
    pub fn key(&self) -> String {
        match self {
            LimitScope::Repository(name) => format!("repository:{name}"),
            LimitScope::Format(format) => format!("format:{}", format.as_str()),
            LimitScope::Every => "every".to_string(),
        }
    }

    /// What a refusal tells the client it was limited on.
    pub fn describe(&self) -> String {
        match self {
            LimitScope::Repository(name) => format!("repository {name}"),
            LimitScope::Format(format) => format.as_str().to_string(),
            LimitScope::Every => "publishing".to_string(),
        }
    }
}

/// The limits of one server: one fallback for every format, the entries that
/// replace it per format, and the entries that replace those per repository.
/// A format with no entry and no fallback is not metered.
#[derive(Debug, Clone, Default)]
pub struct PublishLimits {
    every: Option<PublishLimit>,
    per_format: HashMap<Format, PublishLimit>,
    per_repository: HashMap<String, PublishLimit>,
}

impl PublishLimits {
    pub fn new(
        every: Option<PublishLimit>,
        per_format: HashMap<Format, PublishLimit>,
        per_repository: HashMap<String, PublishLimit>,
    ) -> Self {
        Self {
            every,
            per_format,
            per_repository,
        }
    }

    /// The one limit a publish is counted against: the most specific entry
    /// that exists. A repository entry replaces its format's rather than
    /// adding to it.
    pub fn applicable(
        &self,
        format: Format,
        repository: &str,
    ) -> Option<(LimitScope, PublishLimit)> {
        if let Some(limit) = self.per_repository.get(repository) {
            return Some((LimitScope::Repository(repository.to_string()), *limit));
        }
        if let Some(limit) = self.per_format.get(&format) {
            return Some((LimitScope::Format(format), *limit));
        }
        self.every.map(|limit| (LimitScope::Every, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(max: u32) -> PublishLimit {
        PublishLimit::new(max, 60).expect("a valid limit")
    }

    #[test]
    fn a_limit_is_finite_by_construction() {
        assert_eq!(PublishLimit::new(0, 60), Err(LimitError::Empty));
        assert_eq!(
            PublishLimit::new(MAX_PER_WINDOW + 1, 60),
            Err(LimitError::TooLarge)
        );
        assert_eq!(PublishLimit::new(30, 0), Err(LimitError::Window));
        assert_eq!(
            PublishLimit::new(30, MAX_WINDOW_SECS + 1),
            Err(LimitError::Window)
        );
        assert_eq!(limit(30).max(), 30);
        assert_eq!(limit(30).window_secs(), 60);
    }

    #[test]
    fn a_format_entry_replaces_the_fallback() {
        let limits = PublishLimits::new(
            Some(limit(10)),
            HashMap::from([(Format::Npm, limit(30))]),
            HashMap::new(),
        );
        assert_eq!(
            limits.applicable(Format::Npm, "npm-private"),
            Some((LimitScope::Format(Format::Npm), limit(30)))
        );
        assert_eq!(
            limits.applicable(Format::Cargo, "crates"),
            Some((LimitScope::Every, limit(10)))
        );
    }

    #[test]
    fn a_repository_entry_replaces_its_format() {
        let limits = PublishLimits::new(
            None,
            HashMap::from([(Format::Npm, limit(30))]),
            HashMap::from([("npm-ci".to_string(), limit(600))]),
        );
        assert_eq!(
            limits.applicable(Format::Npm, "npm-ci"),
            Some((LimitScope::Repository("npm-ci".to_string()), limit(600)))
        );
        assert_eq!(
            limits.applicable(Format::Npm, "npm-private"),
            Some((LimitScope::Format(Format::Npm), limit(30)))
        );
    }

    #[test]
    fn a_format_with_no_entry_and_no_fallback_is_not_metered() {
        let limits = PublishLimits::new(
            None,
            HashMap::from([(Format::Npm, limit(30))]),
            HashMap::new(),
        );
        assert_eq!(limits.applicable(Format::Cargo, "crates"), None);
        assert_eq!(
            PublishLimits::default().applicable(Format::Npm, "npm-private"),
            None
        );
    }

    #[test]
    fn a_scope_keys_and_describes_itself() {
        assert_eq!(
            LimitScope::Repository("npm-ci".to_string()).key(),
            "repository:npm-ci"
        );
        assert_eq!(LimitScope::Format(Format::Pypi).key(), "format:pypi");
        assert_eq!(LimitScope::Every.key(), "every");
        assert_eq!(LimitScope::Format(Format::Npm).describe(), "npm");
        assert_eq!(
            LimitScope::Repository("npm-ci".to_string()).describe(),
            "repository npm-ci"
        );
        assert_eq!(LimitScope::Every.describe(), "publishing");
    }
}
