//! What a vulnerability scan concluded: the severities, the findings and the
//! summary a version carries afterwards.
//!
//! The OSV client that fetches the advisories is not here — an advisory
//! record, its CVSS vectors and the HTTP call that retrieved them are the
//! feed adapter's business. What survives the fetch is these three types.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Unknown,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// A feed's own word for how bad something is, when it offers one.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "high" => Some(Self::High),
            "moderate" | "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One finding: a dependency at a version, and the advisory against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulnDetail {
    pub dependency: String,
    pub version: String,
    pub vuln_id: String,
    pub summary: String,
    pub severity: Severity,
    pub score: Option<f64>,
}

/// What a scan of one version concluded, as it is stored and served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub total_deps: usize,
    pub vulnerable_deps: usize,
    pub status: String,
    pub details: Vec<VulnDetail>,
}

impl ScanResult {
    /// Nothing was found, or nothing was looked at: the same answer, because
    /// a disabled scanner must not claim a version is vulnerable.
    pub fn clean() -> Self {
        Self {
            total_deps: 0,
            vulnerable_deps: 0,
            status: "clean".to_string(),
            details: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities_order_from_unknown_up_and_spell_themselves_lowercase() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::Low > Severity::Unknown);
        assert_eq!(Severity::Medium.to_string(), "medium");
        assert_eq!(
            serde_json::to_value(Severity::Critical).unwrap(),
            "critical"
        );
    }

    #[test]
    fn a_feeds_label_reads_back_medium_for_both_of_its_spellings() {
        assert_eq!(Severity::from_label("MODERATE"), Some(Severity::Medium));
        assert_eq!(Severity::from_label("medium"), Some(Severity::Medium));
        assert_eq!(Severity::from_label("whatever"), None);
    }

    #[test]
    fn a_clean_result_claims_nothing() {
        let clean = ScanResult::clean();
        assert_eq!(clean.status, "clean");
        assert_eq!((clean.total_deps, clean.vulnerable_deps), (0, 0));
        assert!(clean.details.is_empty());
    }
}
