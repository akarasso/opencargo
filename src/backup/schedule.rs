//! The in-process schedule: `every` is the period, `at` the phase in UTC.
//! A window missed while the process was down is skipped, never caught up.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveTime, Timelike, Utc};
use tracing::{info, warn};

use super::{Backup, BackupPlan};
use crate::app::lease::LeaseHandle;

const DAY: i64 = 86_400;

/// `HH:MM`, in UTC.
pub fn parse_at(at: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(at, "%H:%M").ok()
}

/// The smallest instant after `now` congruent to `at` modulo `every`, for a
/// period that divides the day.
pub fn next_run(now: DateTime<Utc>, every: Duration, at: NaiveTime) -> DateTime<Utc> {
    let period = (every.as_secs() as i64).clamp(1, DAY);
    let phase = i64::from(at.num_seconds_from_midnight()) % period;
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    let since = (now - midnight).num_seconds();
    let k = (since - phase).div_euclid(period) + 1;
    midnight + chrono::Duration::seconds(phase + k * period)
}

/// Runs a backup at every occurrence, on the lease holder only; never at
/// boot, so a crash loop is not a backup loop.
pub async fn start_backup_schedule(
    backup: Backup,
    plan: Arc<BackupPlan>,
    every: Duration,
    at: NaiveTime,
    lease: LeaseHandle,
) {
    info!(?every, %at, "backup schedule started (UTC)");
    loop {
        let now = backup.clock.now();
        let next = next_run(now, every, at);
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;
        if !lease.held() {
            continue;
        }
        if let Err(e) = backup.run(&plan).await {
            warn!(error = %e, "scheduled backup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn utc(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, h, m, 0).unwrap()
    }

    const HOUR: u64 = 3600;

    #[test]
    fn next_run_is_the_next_instant_congruent_to_at_mod_every() {
        let daily = Duration::from_secs(24 * HOUR);
        assert_eq!(next_run(utc(1, 0), daily, at(3, 0)), utc(3, 0));
        let six = Duration::from_secs(6 * HOUR);
        assert_eq!(next_run(utc(4, 0), six, at(3, 0)), utc(9, 0));
        assert_eq!(next_run(utc(9, 0), six, at(3, 0)), utc(15, 0), "strictly after now");
        assert_eq!(next_run(utc(22, 0), six, at(3, 0)), utc(21, 0) + chrono::Duration::hours(6));
        let hourly = Duration::from_secs(HOUR);
        assert_eq!(next_run(utc(4, 20), hourly, at(3, 0)), utc(5, 0));
    }

    #[test]
    fn schedule_is_utc_and_skips_a_missed_window() {
        let daily = Duration::from_secs(24 * HOUR);
        let next = next_run(utc(4, 0), daily, at(3, 0));
        assert_eq!(next, utc(3, 0) + chrono::Duration::days(1), "tomorrow, not now");
        assert_eq!(parse_at("03:00"), Some(at(3, 0)));
        assert_eq!(parse_at("3am"), None);
    }
}
