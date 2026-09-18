pub mod install_scripts;
pub mod min_release_age;
pub mod osv_severity;
pub mod typosquat;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

use crate::telemetry::vulns::severity::Severity;
use crate::telemetry::vulns::VulnScanner;

use super::age::Age;
use super::{Resolution, RuleVerdict};
use osv_severity::{OsvMemo, OsvSeverity};

/// The rules of one proxy member, all off unless configured; a member with
/// none on records nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub min_release_age: Option<Age>,
    #[serde(deserialize_with = "threshold")]
    pub osv_severity: Option<Severity>,
    pub install_scripts: bool,
    pub typosquat: bool,
    pub fetch_missing_facts: bool,
}

/// `Severity` deserialises `unknown` too, the level of an advisory
/// without a score; as a threshold it would flag every advisory.
fn threshold<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Severity>, D::Error> {
    match Option::<Severity>::deserialize(d)? {
        Some(Severity::Unknown) => Err(serde::de::Error::custom(
            "osv_severity: expected one of low, medium, high, critical",
        )),
        level => Ok(level),
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            min_release_age: None,
            osv_severity: None,
            install_scripts: false,
            typosquat: false,
            fetch_missing_facts: true,
        }
    }
}

impl PolicyConfig {
    pub fn is_empty(&self) -> bool {
        self.min_release_age.is_none()
            && self.osv_severity.is_none()
            && !self.install_scripts
            && !self.typosquat
    }

    pub fn needs_packument(&self) -> bool {
        self.min_release_age.is_some() || self.install_scripts
    }
}

/// A pure, synchronous strategy over a gathered resolution; `None` defers
/// the verdict to the flush.
pub trait Rule: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, cfg: &PolicyConfig) -> bool;
    fn evaluate(
        &self,
        cfg: &PolicyConfig,
        r: &Resolution,
        now: DateTime<Utc>,
    ) -> Option<RuleVerdict>;
}

/// The four strategies, in report order; `osv_severity` shares its memo
/// with the writer's flush.
pub fn all_rules(scanner: Arc<VulnScanner>, memo: Arc<OsvMemo>) -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(min_release_age::MinReleaseAge),
        Box::new(OsvSeverity::new(scanner, memo)),
        Box::new(install_scripts::InstallScripts),
        Box::new(typosquat::Typosquat),
    ]
}

/// One slot per enabled rule, in `rules` order.
pub fn evaluate_all(
    rules: &[Box<dyn Rule>],
    cfg: &PolicyConfig,
    r: &Resolution,
    now: DateTime<Utc>,
) -> Vec<Option<RuleVerdict>> {
    rules
        .iter()
        .filter(|rule| rule.enabled(cfg))
        .map(|rule| rule.evaluate(cfg, r, now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_fetches_missing_facts() {
        let cfg: PolicyConfig = toml::from_str(r#"min_release_age = "48h""#).unwrap();
        assert!(cfg.fetch_missing_facts);
        assert!(!cfg.is_empty());
        assert!(cfg.needs_packument());
        assert!(PolicyConfig::default().is_empty());
        assert!(PolicyConfig::default().fetch_missing_facts);
        let scripts: PolicyConfig = toml::from_str("install_scripts = true").unwrap();
        assert!(scripts.needs_packument());
        let squat: PolicyConfig = toml::from_str("typosquat = true").unwrap();
        assert!(!squat.needs_packument() && !squat.is_empty());
    }

    #[test]
    fn only_enabled_rules_leave_a_slot() {
        let rules = all_rules(
            Arc::new(VulnScanner::new(&crate::config::VulnScanConfig::default()).unwrap()),
            osv_severity::new_memo(),
        );
        let names: Vec<&str> = rules.iter().map(|r| r.name()).collect();
        assert_eq!(
            names,
            [
                "min_release_age",
                "osv_severity",
                "install_scripts",
                "typosquat"
            ]
        );
        let r = Resolution {
            requested_repo: "r".into(),
            member_repo: "m".into(),
            format: crate::domain::Format::Npm,
            name: "lodash".into(),
            version: Some("1.0.0".into()),
            digest: None,
            actor: crate::policy::Actor::of(None),
            published_at: None,
            facts: crate::policy::Facts::default(),
        };
        let cfg = PolicyConfig {
            typosquat: true,
            ..Default::default()
        };
        let verdicts = evaluate_all(&rules, &cfg, &r, Utc::now());
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].as_ref().unwrap().rule, "typosquat");
        assert!(evaluate_all(&rules, &PolicyConfig::default(), &r, Utc::now()).is_empty());
    }

    #[test]
    fn config_rejects_unknown_severity() {
        let err = toml::from_str::<PolicyConfig>(r#"osv_severity = "unknown""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("low, medium, high, critical"), "{err}");
        assert!(toml::from_str::<PolicyConfig>(r#"osv_severity = "severe""#).is_err());
        let high: PolicyConfig = toml::from_str(r#"osv_severity = "high""#).unwrap();
        assert_eq!(high.osv_severity, Some(Severity::High));
        let none: PolicyConfig = toml::from_str("typosquat = true").unwrap();
        assert_eq!(none.osv_severity, None);
    }

    #[test]
    fn config_rejects_unknown_key() {
        let err = toml::from_str::<PolicyConfig>("min_release_age = \"1h\"\nmax_age = \"2h\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_age"), "{err}");
    }
}
