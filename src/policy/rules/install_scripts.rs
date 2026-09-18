use chrono::{DateTime, Utc};

use super::{PolicyConfig, Rule};
use crate::domain::Format;
use crate::policy::{Resolution, RuleVerdict, Verdict};

/// npm only: the fact was gathered from the packument by the recorder.
pub struct InstallScripts;

impl Rule for InstallScripts {
    fn name(&self) -> &'static str {
        "install_scripts"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.install_scripts
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let (verdict, reason) = if r.format != Format::Npm {
            (
                Verdict::NotApplicable,
                format!("{}: install scripts are an npm notion", r.format.as_str()),
            )
        } else {
            match (r.facts.install_scripts, &r.version) {
                (Some(true), _) => (
                    Verdict::WouldBlock,
                    "declares preinstall/install/postinstall".to_string(),
                ),
                (Some(false), _) => (Verdict::Pass, "no install script".to_string()),
                (None, None) => (
                    Verdict::Unknown,
                    format!("version unresolved ({})", r.facts.date_source),
                ),
                (None, Some(_)) => (
                    Verdict::Unknown,
                    format!("packument not read ({})", r.facts.date_source),
                ),
            }
        };
        Some(RuleVerdict::new(self.name(), verdict, reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Actor, Facts};

    fn resolution(
        format: Format,
        version: Option<&str>,
        scripts: Option<bool>,
        source: &'static str,
    ) -> Resolution {
        Resolution {
            requested_repo: "r".into(),
            member_repo: "m".into(),
            format,
            name: "sharp".into(),
            version: version.map(String::from),
            digest: None,
            actor: Actor::of(None),
            published_at: None,
            facts: Facts {
                install_scripts: scripts,
                date_source: source,
            },
        }
    }

    fn run(r: &Resolution) -> RuleVerdict {
        let cfg = PolicyConfig {
            install_scripts: true,
            ..Default::default()
        };
        InstallScripts.evaluate(&cfg, r, Utc::now()).unwrap()
    }

    #[test]
    fn non_npm_not_applicable() {
        for format in [Format::Cargo, Format::Go, Format::Oci] {
            let v = run(&resolution(format, Some("1"), Some(true), "none"));
            assert_eq!(v.verdict, Verdict::NotApplicable, "{format:?}");
            assert!(v.reason.starts_with(format.as_str()), "{}", v.reason);
        }
    }

    #[test]
    fn none_is_unknown_naming_cause() {
        let v = run(&resolution(Format::Npm, Some("1.0.0"), None, "not-fetched"));
        assert_eq!(v.verdict, Verdict::Unknown);
        assert_eq!(v.reason, "packument not read (not-fetched)");
        let v = run(&resolution(Format::Npm, None, None, "filename-unparsed"));
        assert_eq!(v.verdict, Verdict::Unknown);
        assert_eq!(v.reason, "version unresolved (filename-unparsed)");
    }

    #[test]
    fn true_blocks() {
        let v = run(&resolution(Format::Npm, Some("1.0.0"), Some(true), "cache"));
        assert_eq!(v.verdict, Verdict::WouldBlock);
        assert!(v.reason.contains("postinstall"));
        let v = run(&resolution(
            Format::Npm,
            Some("1.0.0"),
            Some(false),
            "cache",
        ));
        assert_eq!(v.verdict, Verdict::Pass);
    }
}
