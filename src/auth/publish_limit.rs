//! The counter behind the domain's publish limits: a sliding window per
//! (scope, account), asked for a verdict at every metered publish.
//!
//! The caller passes `now`, so the window is a deadline and never a sleep.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use crate::domain::limits::{LimitScope, PublishLimit, PublishLimits};
use crate::domain::Format;

/// Above this many keys, one sweep drops the windows that have fully expired.
/// Keys are account names, which an admin creates, but a long-lived server
/// should still not keep every account that ever published.
const SWEEP_ABOVE: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    Refused {
        scope: LimitScope,
        limit: PublishLimit,
        retry_after_secs: u64,
    },
}

impl Admission {
    /// What the client is told, limit included: an account that hits the wall
    /// learns where the wall is.
    pub fn message(&self) -> Option<String> {
        match self {
            Admission::Allowed => None,
            Admission::Refused {
                scope,
                limit,
                retry_after_secs,
            } => Some(format!(
                "publish rate limit reached for {}: {} per {}s, retry in {retry_after_secs}s",
                scope.describe(),
                limit.max(),
                limit.window_secs(),
            )),
        }
    }
}

pub struct PublishMeter {
    limits: PublishLimits,
    windows: Mutex<HashMap<String, Vec<DateTime<Utc>>>>,
}

impl PublishMeter {
    pub fn new(limits: PublishLimits) -> Self {
        Self {
            limits,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Count one publish against the applicable limit, or refuse it. A
    /// refusal records nothing, so a client that keeps knocking never pushes
    /// its own window further out.
    pub fn admit(
        &self,
        account: &str,
        format: Format,
        repository: &str,
        now: DateTime<Utc>,
    ) -> Admission {
        let Some((scope, limit)) = self.limits.applicable(format, repository) else {
            return Admission::Allowed;
        };
        let window = Duration::seconds(limit.window_secs() as i64);
        let cutoff = now - window;

        // The state is a best-effort counter, usable as it stands after a panic.
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if windows.len() > SWEEP_ABOVE {
            windows.retain(|_, hits| hits.iter().any(|hit| *hit > cutoff));
        }

        let hits = windows.entry(format!("{}|{account}", scope.key())).or_default();
        hits.retain(|hit| *hit > cutoff);
        if (hits.len() as u64) < u64::from(limit.max()) {
            hits.push(now);
            return Admission::Allowed;
        }

        let frees_at = hits.iter().min().copied().unwrap_or(now) + window;
        let retry_after_secs = (frees_at - now).num_milliseconds().div_euclid(1000).max(0) as u64 + 1;
        Admission::Refused {
            scope,
            limit,
            retry_after_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("a valid instant")
    }

    fn limit(max: u32) -> PublishLimit {
        PublishLimit::new(max, 60).expect("a valid limit")
    }

    fn meter(limits: PublishLimits) -> PublishMeter {
        PublishMeter::new(limits)
    }

    fn per_format(max: u32) -> PublishLimits {
        PublishLimits::new(None, HashMap::from([(Format::Npm, limit(max))]), HashMap::new())
    }

    #[test]
    fn the_limit_is_the_last_publish_admitted() {
        let meter = meter(per_format(3));
        for _ in 0..3 {
            assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        }
        let refused = meter.admit("ci", Format::Npm, "npm-private", at(0));
        assert!(matches!(refused, Admission::Refused { .. }));
        assert_eq!(
            refused.message().as_deref(),
            Some("publish rate limit reached for npm: 3 per 60s, retry in 61s")
        );
    }

    #[test]
    fn a_refusal_records_nothing() {
        let meter = meter(per_format(1));
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        for second in 1..30 {
            let refused = meter.admit("ci", Format::Npm, "npm-private", at(second));
            let Admission::Refused { retry_after_secs, .. } = refused else {
                panic!("the window is full until t+60");
            };
            assert_eq!(retry_after_secs, (60 - second) as u64 + 1);
        }
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(61)), Admission::Allowed);
    }

    #[test]
    fn the_window_slides() {
        let meter = meter(per_format(2));
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(30)), Admission::Allowed);
        assert!(matches!(
            meter.admit("ci", Format::Npm, "npm-private", at(59)),
            Admission::Refused { .. }
        ));
        // t+61 has dropped the publish of t+0 only.
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(61)), Admission::Allowed);
        assert!(matches!(
            meter.admit("ci", Format::Npm, "npm-private", at(62)),
            Admission::Refused { .. }
        ));
    }

    #[test]
    fn one_account_never_spends_another() {
        let meter = meter(per_format(1));
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        assert_eq!(meter.admit("dev", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        assert!(matches!(
            meter.admit("ci", Format::Npm, "npm-private", at(0)),
            Admission::Refused { .. }
        ));
    }

    #[test]
    fn a_repository_entry_counts_apart_from_its_format() {
        let limits = PublishLimits::new(
            None,
            HashMap::from([(Format::Npm, limit(1))]),
            HashMap::from([("npm-ci".to_string(), limit(2))]),
        );
        let meter = meter(limits);
        assert_eq!(meter.admit("ci", Format::Npm, "npm-private", at(0)), Admission::Allowed);
        assert!(matches!(
            meter.admit("ci", Format::Npm, "npm-private", at(0)),
            Admission::Refused { .. }
        ));
        for _ in 0..2 {
            assert_eq!(meter.admit("ci", Format::Npm, "npm-ci", at(0)), Admission::Allowed);
        }
        let refused = meter.admit("ci", Format::Npm, "npm-ci", at(0));
        assert_eq!(
            refused.message().as_deref(),
            Some("publish rate limit reached for repository npm-ci: 2 per 60s, retry in 61s")
        );
    }

    #[test]
    fn a_format_with_no_limit_is_never_refused() {
        let meter = meter(per_format(1));
        for _ in 0..100 {
            assert_eq!(meter.admit("ci", Format::Cargo, "crates", at(0)), Admission::Allowed);
        }
        assert_eq!(Admission::Allowed.message(), None);
    }

    #[test]
    fn the_map_is_swept_once_it_grows() {
        let meter = meter(PublishLimits::new(Some(limit(1)), HashMap::new(), HashMap::new()));
        for n in 0..=SWEEP_ABOVE {
            assert_eq!(meter.admit(&format!("ci{n}"), Format::Npm, "npm-private", at(0)), Admission::Allowed);
        }
        assert_eq!(meter.admit("late", Format::Npm, "npm-private", at(120)), Admission::Allowed);
        let held = meter.windows.lock().expect("the counter").len();
        assert_eq!(held, 1, "the expired windows are gone, the fresh one stays");
    }
}
