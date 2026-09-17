use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{PolicyConfig, Rule};
use crate::policy::memo::Memo;
use crate::policy::{Resolution, RuleVerdict, Verdict};
use crate::telemetry::vulns::severity::Severity;
use crate::telemetry::vulns::{VulnDetail, VulnScanner};

pub const OSV_MEMO: usize = 4096;
const OSV_TTL: Duration = Duration::from_secs(3600);
const RULE: &str = "osv_severity";

/// The artifact's own top finding, `None` when clean; the threshold stays
/// out so one entry serves members with different levels.
#[derive(Debug, Clone)]
pub struct OsvFinding {
    pub top: Option<(String, Severity)>,
    pub at: DateTime<Utc>,
}

impl OsvFinding {
    pub fn from_details(details: &[VulnDetail], now: DateTime<Utc>) -> Self {
        let top = details
            .iter()
            .max_by_key(|d| d.severity)
            .map(|d| (d.vuln_id.clone(), d.severity));
        Self { top, at: now }
    }

    fn fresh(&self, now: DateTime<Utc>) -> bool {
        (now - self.at).to_std().is_ok_and(|age| age < OSV_TTL)
    }
}

pub type Triple = (String, String, String);
pub type OsvMemo = Mutex<Memo<Triple, OsvFinding>>;

pub fn new_memo() -> Arc<OsvMemo> {
    Arc::new(Mutex::new(Memo::new(OSV_MEMO)))
}

fn memo_hit(memo: &OsvMemo, key: &Triple, now: DateTime<Utc>) -> Option<OsvFinding> {
    memo.lock()
        .unwrap()
        .get_mut(key)
        .filter(|f| f.fresh(now))
        .cloned()
}

fn triple(r: &Resolution) -> Option<Triple> {
    Some((
        r.format.osv_ecosystem()?.to_string(),
        r.name.clone(),
        r.version.clone()?,
    ))
}

/// The one verdict both stages compute from a finding.
pub fn verdict(level: Severity, finding: &OsvFinding) -> RuleVerdict {
    let (verdict, reason) = match &finding.top {
        Some((id, sev)) if *sev >= level => (Verdict::WouldBlock, format!("{id} {sev} >= {level}")),
        Some((id, Severity::Unknown)) => (
            Verdict::Pass,
            format!("{id} has no severity, below {level}"),
        ),
        Some((id, sev)) => (Verdict::Pass, format!("{id} {sev} < {level}")),
        None => (Verdict::Pass, "no known vulnerability".to_string()),
    };
    RuleVerdict::new(RULE, verdict, reason)
}

/// Per event: applicability and the memo; a miss defers to the flush.
pub struct OsvSeverity {
    scanner: Arc<VulnScanner>,
    memo: Arc<OsvMemo>,
}

impl OsvSeverity {
    pub fn new(scanner: Arc<VulnScanner>, memo: Arc<OsvMemo>) -> Self {
        Self { scanner, memo }
    }
}

impl Rule for OsvSeverity {
    fn name(&self) -> &'static str {
        RULE
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.osv_severity.is_some()
    }

    fn evaluate(
        &self,
        cfg: &PolicyConfig,
        r: &Resolution,
        now: DateTime<Utc>,
    ) -> Option<RuleVerdict> {
        let level = cfg.osv_severity?;
        if r.format.osv_ecosystem().is_none() {
            return Some(RuleVerdict::new(
                RULE,
                Verdict::NotApplicable,
                format!("{}: no osv ecosystem", r.format.as_str()),
            ));
        }
        let Some(key) = triple(r) else {
            return Some(RuleVerdict::new(
                RULE,
                Verdict::Unknown,
                "version unresolved",
            ));
        };
        if !self.scanner.enabled() {
            return Some(RuleVerdict::new(
                RULE,
                Verdict::Unknown,
                "osv scanning disabled",
            ));
        }
        memo_hit(&self.memo, &key, now).map(|finding| verdict(level, &finding))
    }
}

type Row = (Resolution, Vec<Option<RuleVerdict>>);

/// Per flush: the deferred triples, deduplicated and grouped by ecosystem,
/// one `assess_batch` per group; an error marks its group `unknown`.
pub async fn evaluate_batch(
    scanner: &VulnScanner,
    memo: &OsvMemo,
    level_of: impl Fn(&Resolution) -> Option<Severity>,
    rows: &mut [Row],
) {
    let now = Utc::now();
    let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut seen = HashSet::new();
    for (r, _) in rows.iter().filter(|(_, v)| v.contains(&None)) {
        let Some(key) = triple(r) else { continue };
        if memo_hit(memo, &key, now).is_none() && seen.insert(key.clone()) {
            groups.entry(key.0).or_default().push((key.1, key.2));
        }
    }
    let mut failed: HashMap<String, String> = HashMap::new();
    for (ecosystem, deps) in groups {
        match scanner.assess_batch(&ecosystem, &deps).await {
            Ok(findings) => {
                let mut memo = memo.lock().unwrap();
                for ((name, version), details) in deps.into_iter().zip(findings) {
                    memo.insert(
                        (ecosystem.clone(), name, version),
                        OsvFinding::from_details(&details, now),
                    );
                }
            }
            Err(e) => {
                failed.insert(ecosystem, e.to_string());
            }
        }
    }
    for (r, verdicts) in rows.iter_mut() {
        let Some(slot) = verdicts.iter_mut().find(|v| v.is_none()) else {
            continue;
        };
        let Some(level) = level_of(r) else { continue };
        let Some(key) = triple(r) else { continue };
        *slot = Some(match (failed.get(&key.0), memo_hit(memo, &key, now)) {
            (_, Some(finding)) => verdict(level, &finding),
            (Some(e), None) => {
                RuleVerdict::new(RULE, Verdict::Unknown, format!("osv unreachable: {e}"))
            }
            (None, None) => RuleVerdict::new(RULE, Verdict::Unknown, "osv answered nothing"),
        });
    }
}

#[cfg(test)]
#[path = "osv_severity_tests.rs"]
mod tests;
