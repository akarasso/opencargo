//! The policy report: what the proxy resolved, what each rule said about it,
//! and the counts the report screen shows.
//!
//! Two methods compare a column against a clock, and both take that clock as
//! a parameter rather than letting the database read its own — the retention
//! predicate is the one place a dialect's date arithmetic would otherwise be
//! unportable, and the one place it is not observable from a test.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::RuleVerdict;
use crate::error::StoreError;

/// One resolution to record, with the verdicts that belong to it.
pub struct NewResolution<'a> {
    pub requested_repo: &'a str,
    pub member_repo: &'a str,
    pub format: &'a str,
    pub name: &'a str,
    pub version: Option<&'a str>,
    pub digest: Option<&'a str>,
    /// When upstream says the artifact was published, when it says at all.
    pub published_at: Option<DateTime<Utc>>,
    pub date_source: &'a str,
    /// Display only; `user_id` is the identity an erasure works from.
    pub actor: &'a str,
    pub actor_kind: &'a str,
    pub user_id: Option<i64>,
    pub verdicts: &'a [RuleVerdict],
}

/// Whose rows `/me/policy` shows: a database user's, or the config token's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    User(i64),
    Static,
}

/// `repo` matches the requested or the member repository; `rule` narrows
/// every number and verdict to that rule's own row.
#[derive(Debug, Clone, Copy)]
pub struct ReportFilter<'a> {
    pub since: DateTime<Utc>,
    pub repo: Option<&'a str>,
    pub rule: Option<&'a str>,
    pub subject: Option<Subject>,
}

/// Which rows of the window the totals cover: `after < id <= upto`, so a
/// snapshot and the delta on top of it never count a row twice.
#[derive(Debug, Clone, Copy)]
pub struct IdRange {
    pub after: i64,
    pub upto: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RuleTotals {
    pub would_block: u64,
    pub unknown: u64,
    pub pass: u64,
    pub not_applicable: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub resolutions: u64,
    pub would_block: u64,
    pub unknown: u64,
    pub by_rule: BTreeMap<String, RuleTotals>,
}

impl Totals {
    /// Adds the counts of a later id range.
    pub fn absorb(&mut self, delta: Totals) {
        self.resolutions += delta.resolutions;
        self.would_block += delta.would_block;
        self.unknown += delta.unknown;
        for (rule, d) in delta.by_rule {
            let t = self.by_rule.entry(rule).or_default();
            t.would_block += d.would_block;
            t.unknown += d.unknown;
            t.pass += d.pass;
            t.not_applicable += d.not_applicable;
        }
    }
}

/// One recorded resolution as the report lists it.
#[derive(Debug, Clone)]
pub struct ResolutionRow {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub requested_repo: String,
    pub member_repo: String,
    pub format: String,
    pub name: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    /// Where the publication date came from, which is what a resolution with
    /// no date has to explain.
    pub date_source: String,
    pub actor: String,
    pub actor_kind: String,
    pub user_id: Option<i64>,
    pub published_at: Option<DateTime<Utc>>,
    pub would_block: bool,
    pub unknown: bool,
}

/// One verdict of one listed resolution.
#[derive(Debug, Clone)]
pub struct VerdictRow {
    pub resolution_id: i64,
    pub rule: String,
    pub verdict: String,
    pub reason: String,
}

#[async_trait]
pub trait PolicyStore: Send + Sync {
    /// One transaction for the whole batch; a resolution and its verdicts
    /// land together or not at all. `now` fills `created_at`, so no column
    /// default ever fires.
    async fn insert_batch(
        &self,
        rows: &[NewResolution<'_>],
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>, StoreError>;

    /// The newest resolution id, the upper bound of a totals snapshot.
    async fn max_id(&self) -> Result<i64, StoreError>;

    /// Totals over the rows of `range` inside the filter's window.
    async fn totals(
        &self,
        filter: &ReportFilter<'_>,
        range: IdRange,
    ) -> Result<Totals, StoreError>;

    /// Newest first, one page at a time.
    async fn resolutions(
        &self,
        filter: &ReportFilter<'_>,
        page: i64,
        size: i64,
    ) -> Result<Vec<ResolutionRow>, StoreError>;

    /// The verdicts of the page's rows, that rule's alone when `rule` is set.
    async fn verdicts_for(
        &self,
        ids: &[i64],
        rule: Option<&str>,
    ) -> Result<Vec<VerdictRow>, StoreError>;

    /// Retention. `now` is the caller's clock, never the database's: the
    /// cut-off is a bound parameter so a second dialect has no date
    /// arithmetic of its own to get right.
    async fn delete_older_than(
        &self,
        days: u64,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError>;

    /// Erasure by identity, never by label: a homonymous token of another
    /// user keeps its rows. Chunked by the adapter, and deliberately not part
    /// of the transaction that deletes the user — a day of resolutions must
    /// not hold a single writer.
    async fn erase_user(&self, user_id: i64) -> Result<u64, StoreError>;
}
