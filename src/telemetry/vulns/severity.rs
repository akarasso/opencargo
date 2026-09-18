//! How an OSV record's scoring vectors become a [`Severity`].
//!
//! The enum itself is the domain's; what is here is the CVSS parsing and the
//! precedence rule, which are the feed's own vocabulary.

use std::str::FromStr;

use serde::Deserialize;

/// The enum is the domain's; re-exported because this is the path the OSV
/// client and the report tests name it by.
pub use crate::domain::Severity;

/// One entry of an OSV record's `severity` array: a typed scoring vector.
#[derive(Debug, Clone, Deserialize)]
pub struct OsvSeverityEntry {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub score: Option<String>,
}

fn from_cvss(s: cvss::Severity) -> Severity {
    match s {
        cvss::Severity::Critical => Severity::Critical,
        cvss::Severity::High => Severity::High,
        cvss::Severity::Medium => Severity::Medium,
        cvss::Severity::Low => Severity::Low,
        cvss::Severity::None => Severity::Unknown,
    }
}

/// Severity and score of one advisory. Precedence: a `MAL-` id is critical
/// with no score; a `database_specific.severity` label wins over vectors;
/// else the highest-scoring CVSS 3.x/4.0 vector; CVSS 2 or nothing parsable
/// is unknown.
pub fn classify(
    id: &str,
    database_specific: Option<&serde_json::Value>,
    entries: &[OsvSeverityEntry],
) -> (Severity, Option<f64>) {
    if id.starts_with("MAL-") {
        return (Severity::Critical, None);
    }
    let best = best_vector(entries);
    let label = database_specific
        .and_then(|d| d.get("severity"))
        .and_then(|s| s.as_str())
        .and_then(Severity::from_label);
    match (label, best) {
        (Some(sev), best) => (sev, best.map(|(_, score)| score)),
        (None, Some((sev, score))) => (sev, Some(score)),
        (None, None) => (Severity::Unknown, None),
    }
}

fn best_vector(entries: &[OsvSeverityEntry]) -> Option<(Severity, f64)> {
    entries
        .iter()
        .filter(|e| matches!(e.kind.as_deref(), Some("CVSS_V4" | "CVSS_V3")))
        .filter_map(|e| cvss::Cvss::from_str(e.score.as_deref()?).ok())
        .map(|v| (from_cvss(v.severity()), v.score()))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The enum is the domain's; re-exported because this is the path the OSV
/// client and the report tests name it by.
pub use crate::domain::Severity;
    use serde_json::json;

    fn entry(kind: &str, score: &str) -> OsvSeverityEntry {
        OsvSeverityEntry {
            kind: Some(kind.to_string()),
            score: Some(score.to_string()),
        }
    }

    const V3_CRITICAL: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";
    const V3_MEDIUM: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N";
    const V4_HIGH: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:N/VA:N/SC:N/SI:N/SA:N";

    #[test]
    fn classify_precedence() {
        let mal = classify("MAL-2024-1", None, &[entry("CVSS_V3", V3_MEDIUM)]);
        assert_eq!(mal, (Severity::Critical, None));

        let label = json!({ "severity": "high" });
        let (sev, score) = classify("GHSA-1", Some(&label), &[entry("CVSS_V3", V3_CRITICAL)]);
        assert_eq!(sev, Severity::High);
        assert_eq!(score, Some(9.8));

        let moderate = json!({ "severity": "MODERATE" });
        assert_eq!(
            classify("GHSA-2", Some(&moderate), &[]),
            (Severity::Medium, None)
        );

        let (sev, score) = classify("GHSA-3", None, &[entry("CVSS_V3", V3_CRITICAL)]);
        assert_eq!((sev, score), (Severity::Critical, Some(9.8)));

        let (sev, score) = classify("GHSA-4", None, &[entry("CVSS_V3", V3_MEDIUM)]);
        assert_eq!((sev, score), (Severity::Medium, Some(5.3)));

        let (sev, score) = classify("GHSA-5", None, &[entry("CVSS_V4", V4_HIGH)]);
        assert_eq!(sev, Severity::High);
        assert!(score.is_some_and(|s| (7.0..9.0).contains(&s)));

        let both = [entry("CVSS_V3", V3_MEDIUM), entry("CVSS_V4", V4_HIGH)];
        assert_eq!(classify("GHSA-6", None, &both).0, Severity::High);

        let v2 = [entry("CVSS_V2", "AV:N/AC:L/Au:N/C:P/I:P/A:P")];
        assert_eq!(classify("GHSA-7", None, &v2), (Severity::Unknown, None));

        let garbage = [entry("CVSS_V3", "not a vector")];
        assert_eq!(
            classify("GHSA-8", None, &garbage),
            (Severity::Unknown, None)
        );

        let unknown_label = json!({ "severity": "whatever" });
        assert_eq!(
            classify("GHSA-9", Some(&unknown_label), &[]),
            (Severity::Unknown, None)
        );
        assert!(Severity::Critical > Severity::High && Severity::Low > Severity::Unknown);
    }
}
