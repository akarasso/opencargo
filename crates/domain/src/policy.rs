//! What a policy rule decided about one resolution.
//!
//! A verdict is a value, not a row and not a status: the rules produce it
//! synchronously, the report store writes it, and the HTTP adapter spells it.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    WouldBlock,
    Unknown,
    NotApplicable,
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::WouldBlock => "would_block",
            Verdict::Unknown => "unknown",
            Verdict::NotApplicable => "not_applicable",
        }
    }
}

/// One rule's answer, carrying why it answered that way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleVerdict {
    pub rule: &'static str,
    pub verdict: Verdict,
    pub reason: String,
}

impl RuleVerdict {
    pub fn new(rule: &'static str, verdict: Verdict, reason: impl Into<String>) -> Self {
        Self {
            rule,
            verdict,
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four spellings are the report's vocabulary and the column's
    /// values; a rename here is a schema change, not a refactor.
    #[test]
    fn verdicts_spell_themselves_the_way_the_column_stores_them() {
        assert_eq!(Verdict::Pass.as_str(), "pass");
        assert_eq!(Verdict::WouldBlock.as_str(), "would_block");
        assert_eq!(Verdict::Unknown.as_str(), "unknown");
        assert_eq!(Verdict::NotApplicable.as_str(), "not_applicable");
        for v in [
            Verdict::Pass,
            Verdict::WouldBlock,
            Verdict::Unknown,
            Verdict::NotApplicable,
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), v.as_str());
        }
    }
}
