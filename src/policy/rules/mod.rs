use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::telemetry::vulns::severity::Severity;

use super::age::Age;
use super::{Resolution, RuleVerdict};

/// The rules of one proxy member, all off unless configured; a member with
/// none on records nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub min_release_age: Option<Age>,
    pub osv_severity: Option<Severity>,
    pub install_scripts: bool,
    pub typosquat: bool,
    pub fetch_missing_facts: bool,
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

pub fn all_rules() -> Vec<Box<dyn Rule>> {
    Vec::new()
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
    fn config_rejects_unknown_key() {
        let err = toml::from_str::<PolicyConfig>("min_release_age = \"1h\"\nmax_age = \"2h\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_age"), "{err}");
    }
}
