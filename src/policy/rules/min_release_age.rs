use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{PolicyConfig, Rule};
use crate::domain::{Format, RuleVerdict, Verdict};
use crate::policy::{Age, Resolution};

/// The formats whose upstream protocol carries no per-version publication
/// date: the rule can only ever answer `not_applicable` there, and the
/// startup check says so.
pub fn undated(format: Format) -> bool {
    matches!(format, Format::Maven)
}

pub struct MinReleaseAge;

impl Rule for MinReleaseAge {
    fn name(&self) -> &'static str {
        "min_release_age"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.min_release_age.is_some()
    }

    fn evaluate(
        &self,
        cfg: &PolicyConfig,
        r: &Resolution,
        now: DateTime<Utc>,
    ) -> Option<RuleVerdict> {
        let min = cfg.min_release_age?;
        if undated(r.format) {
            return Some(RuleVerdict::new(
                self.name(),
                Verdict::NotApplicable,
                format!("{}: upstream carries no per-version publication date", r.format.as_str()),
            ));
        }
        let verdict = match r.published_at {
            None => RuleVerdict::new(
                self.name(),
                Verdict::Unknown,
                format!("no publish date ({})", r.facts.date_source),
            ),
            Some(at) => dated(self.name(), min, at, now),
        };
        Some(verdict)
    }
}

/// A future date (clock skew) is age zero, hence `WouldBlock`.
fn dated(rule: &'static str, min: Age, at: DateTime<Utc>, now: DateTime<Utc>) -> RuleVerdict {
    let age = (now - at).to_std().unwrap_or(Duration::ZERO);
    if at > now {
        return RuleVerdict::new(
            rule,
            Verdict::WouldBlock,
            format!("published in the future (clock skew?), threshold {min}"),
        );
    }
    let ago = Age::approx(age);
    if age < min.duration() {
        RuleVerdict::new(
            rule,
            Verdict::WouldBlock,
            format!("published {ago} ago, threshold {min}"),
        )
    } else {
        RuleVerdict::new(rule, Verdict::Pass, format!("published {ago} ago"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Format;
    use crate::policy::{Actor, Facts};

    fn resolution(published_at: Option<DateTime<Utc>>, date_source: &'static str) -> Resolution {
        Resolution {
            requested_repo: "r".into(),
            member_repo: "m".into(),
            format: Format::Npm,
            name: "left-pad".into(),
            version: Some("1.0.0".into()),
            digest: None,
            actor: Actor::of(None),
            published_at,
            facts: Facts {
                install_scripts: None,
                date_source,
                mcp: None,
            },
        }
    }

    fn cfg(age: &str) -> PolicyConfig {
        PolicyConfig {
            min_release_age: Some(age.parse().unwrap()),
            ..Default::default()
        }
    }

    fn run(cfg: &PolicyConfig, r: &Resolution) -> RuleVerdict {
        MinReleaseAge.evaluate(cfg, r, Utc::now()).unwrap()
    }

    #[test]
    fn unknown_without_date() {
        let v = run(&cfg("48h"), &resolution(None, "not-in-packument"));
        assert_eq!(v.verdict, Verdict::Unknown);
        assert_eq!(v.reason, "no publish date (not-in-packument)");
        assert_eq!(v.rule, "min_release_age");
        assert!(MinReleaseAge
            .evaluate(
                &PolicyConfig::default(),
                &resolution(None, "none"),
                Utc::now()
            )
            .is_none());
    }

    #[test]
    fn young_is_would_block() {
        let at = Utc::now() - chrono::Duration::hours(2);
        let v = run(&cfg("48h"), &resolution(Some(at), "fetch"));
        assert_eq!(v.verdict, Verdict::WouldBlock);
        assert_eq!(v.reason, "published 2h ago, threshold 48h");
    }

    #[test]
    fn old_passes() {
        let at = Utc::now() - chrono::Duration::days(30);
        let v = run(&cfg("48h"), &resolution(Some(at), "cache"));
        assert_eq!(v.verdict, Verdict::Pass);
        assert_eq!(v.reason, "published 30d ago");
    }

    #[test]
    fn future_date_is_would_block() {
        let at = Utc::now() + chrono::Duration::hours(1);
        let v = run(&cfg("1s"), &resolution(Some(at), "fetch"));
        assert_eq!(v.verdict, Verdict::WouldBlock);
        assert!(v.reason.contains("future"), "{}", v.reason);
    }

    #[test]
    fn maven_is_not_applicable_even_when_dated() {
        let mut r = resolution(Some(Utc::now() - chrono::Duration::hours(2)), "none");
        r.format = Format::Maven;
        let v = run(&cfg("48h"), &r);
        assert_eq!(v.verdict, Verdict::NotApplicable);
        assert_eq!(v.reason, "maven: upstream carries no per-version publication date");
        assert!(undated(Format::Maven));
        for format in [Format::Npm, Format::Cargo, Format::Go, Format::Pypi, Format::Nuget, Format::Oci, Format::Raw] {
            assert!(!undated(format), "{format:?}");
        }
    }

    #[test]
    fn seven_days_from_config_is_seven_days() {
        let cfg: PolicyConfig = toml::from_str(r#"min_release_age = "7d""#).unwrap();
        let six = Utc::now() - chrono::Duration::days(6);
        assert_eq!(
            run(&cfg, &resolution(Some(six), "fetch")).verdict,
            Verdict::WouldBlock
        );
        let eight = Utc::now() - chrono::Duration::days(8);
        assert_eq!(
            run(&cfg, &resolution(Some(eight), "fetch")).verdict,
            Verdict::Pass
        );
        assert_eq!(
            cfg.min_release_age.unwrap().duration(),
            Duration::from_secs(604_800)
        );
    }
}
