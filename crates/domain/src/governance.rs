//! Governance of what a repository distributes: allow rules over names,
//! approvals bound to a fingerprinted surface, and the gate that turns both
//! into what is served. Nothing here knows a wire format or a hash function:
//! a surface is a pair of digests the caller computed.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::DomainError;

/// What a repository's rules and approvals do to what it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
    /// Serve everything; nothing is flagged.
    Off,
    /// Serve everything, flagging what an approval or a rule would refuse.
    #[default]
    Warn,
    /// Remove what an approval or a rule refuses.
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

impl Effect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::Deny => "deny",
        }
    }
}

impl FromStr for Effect {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        match s {
            "allow" => Ok(Effect::Allow),
            "deny" => Ok(Effect::Deny),
            other => Err(DomainError::InvalidName(format!("invalid rule effect: {other}"))),
        }
    }
}

/// An exact name, or a prefix ending in `*` right after a `/` or a `.`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowRule {
    pub pattern: String,
    pub effect: Effect,
}

impl AllowRule {
    pub fn new(pattern: &str, effect: Effect) -> Result<Self, DomainError> {
        validate_pattern(pattern)?;
        Ok(Self {
            pattern: pattern.to_string(),
            effect,
        })
    }

    /// The prefix a `*` rule matches, `None` for an exact rule.
    pub fn prefix(&self) -> Option<&str> {
        self.pattern.strip_suffix('*')
    }

    pub fn matches(&self, name: &str) -> bool {
        match self.prefix() {
            Some(prefix) => name.starts_with(prefix),
            None => name == self.pattern,
        }
    }
}

/// No general globbing: one trailing `*`, after a boundary, or none.
pub fn validate_pattern(pattern: &str) -> Result<(), DomainError> {
    let refuse = || DomainError::InvalidName(format!("invalid allow rule pattern: '{pattern}'"));
    if pattern.is_empty() || pattern.len() > 256 || pattern.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refuse());
    }
    match pattern.strip_suffix('*') {
        Some(prefix) if prefix.contains('*') => Err(refuse()),
        Some(prefix) if !(prefix.ends_with('/') || prefix.ends_with('.')) => Err(refuse()),
        Some(_) => Ok(()),
        None if pattern.contains('*') => Err(refuse()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_is_exact_or_one_star_after_a_boundary() {
        for good in ["io.github.acme/x", "io.github.acme/*", "com.stripe.*", "deploy-runbook"] {
            assert!(validate_pattern(good).is_ok(), "{good}");
        }
        for bad in ["", "*", "io.github.acme*", "io.*/x", "a/**", "a b/*", "io.github/x*"] {
            assert!(validate_pattern(bad).is_err(), "{bad}");
        }
        let rule = AllowRule::new("io.github.acme/*", Effect::Allow).unwrap();
        assert!(rule.matches("io.github.acme/server"));
        assert!(!rule.matches("io.github.acmecorp/server"));
    }

    #[test]
    fn modes_and_effects_spell_themselves_lowercase() {
        assert_eq!(serde_json::to_string(&GateMode::Hide).unwrap(), "\"hide\"");
        assert_eq!(GateMode::default(), GateMode::Warn);
        assert_eq!("deny".parse::<Effect>().unwrap(), Effect::Deny);
        assert!("maybe".parse::<Effect>().is_err());
    }
}
