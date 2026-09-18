//! MCP governance state: server records, the surfaces each one answered,
//! the findings on them, and every repository's allow rules, suppressions
//! and approvals.
//!
//! Every write recomputes what a page serves in the same transaction: the
//! current surface of a version, its live-set finding counts, and each
//! repository's endpoint verdict. A reader never aggregates.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::governance::{AllowRule, Decision, Drift, Effect};
use crate::error::StoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SurfaceSource {
    Declared,
    Probe,
    Attested,
}

impl SurfaceSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            SurfaceSource::Declared => "declared",
            SurfaceSource::Probe => "probe",
            SurfaceSource::Attested => "attested",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "declared" => Some(SurfaceSource::Declared),
            "probe" => Some(SurfaceSource::Probe),
            "attested" => Some(SurfaceSource::Attested),
            _ => None,
        }
    }
}

/// A scan result as stored: the text that fired, where, and how sure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFinding {
    pub pattern: String,
    pub high: bool,
    pub promoted_by: Option<String>,
    pub field: String,
    pub tool: String,
    pub span: (i64, i64),
    pub excerpt: String,
}

/// A fingerprinted answer of one endpoint, and what the scan found in it.
#[derive(Debug, Clone)]
pub struct NewSurface {
    pub source: SurfaceSource,
    /// The endpoint identity; empty for the declared record and for stdio.
    pub remote_url: String,
    pub tools_json: Option<String>,
    pub tools_sha256: Option<String>,
    pub permissions_sha256: String,
    pub combined_sha256: String,
    pub captured_by: Option<String>,
    pub findings: Vec<NewFinding>,
}

/// One server record as sync or publish writes it. Every column but the
/// envelope is derived from it by the caller.
#[derive(Debug, Clone)]
pub struct RecordWrite {
    pub repository: i64,
    pub name: String,
    pub version: String,
    pub hosted: bool,
    pub envelope_json: String,
    pub schema_url: Option<String>,
    pub status: String,
    pub status_message: Option<String>,
    pub status_changed_at: Option<String>,
    pub is_latest: bool,
    /// Every other version of the name stops being latest, in the same
    /// transaction: publication order, the registry of record's rule.
    pub take_latest: bool,
    pub published_at: Option<String>,
    pub upstream_updated_at: Option<String>,
    pub package_transports: String,
    pub remote_transports: String,
    /// The record's `remotes[]` urls in declaration order.
    pub remote_urls: Vec<String>,
    pub declared: NewSurface,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upserted {
    pub version_id: i64,
    /// The stored envelope differs from what was there, or nothing was.
    pub changed: bool,
}

/// A probe attempt: a surface when it answered, an error when not.
#[derive(Debug, Clone)]
pub struct ProbeRun {
    pub version_id: i64,
    pub remote_url: String,
    pub protocol_version: Option<String>,
    pub outcome: Result<NewSurface, String>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRow {
    pub id: i64,
    pub version_id: i64,
    pub source: SurfaceSource,
    pub remote_url: String,
    pub remote_ordinal: i64,
    pub tools_json: Option<String>,
    pub tools_sha256: Option<String>,
    pub permissions_sha256: String,
    pub combined_sha256: String,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingRow {
    pub id: i64,
    pub subject: Subject,
    pub pattern: String,
    pub high: bool,
    pub promoted_by: Option<String>,
    pub field: String,
    pub tool: String,
    pub span: (i64, i64),
    pub excerpt: String,
    pub suppressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Surface(i64),
    Skill(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRunRow {
    pub remote_url: String,
    pub ran_at: DateTime<Utc>,
    pub ok: bool,
    pub protocol_version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub id: i64,
    pub repository: i64,
    pub name: String,
    pub version: String,
    pub remote_url: String,
    pub permissions_sha256: String,
    pub tools_sha256: Option<String>,
    pub surface_id: Option<i64>,
    pub decision: Decision,
    pub decided_by: String,
    pub decided_at: DateTime<Utc>,
    pub note: Option<String>,
}

/// One decision about one endpoint of a version, or about a skill.
#[derive(Debug, Clone)]
pub struct NewApproval {
    pub repository: i64,
    pub skill: bool,
    pub name: String,
    pub version: String,
    pub remote_url: String,
    pub permissions_sha256: String,
    pub tools_sha256: Option<String>,
    pub combined_sha256: String,
    pub surface_id: Option<i64>,
    pub decision: Decision,
    pub decided_by: String,
    pub note: Option<String>,
    pub now: DateTime<Utc>,
}

/// A server version as a page serves it to one addressed repository: the
/// member's row, the current surface, and the addressed repository's
/// verdict, approval and counts falling back to the member's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub version_id: i64,
    pub member: i64,
    pub name: String,
    pub version: String,
    pub hosted: bool,
    pub envelope_json: String,
    pub status: String,
    pub is_latest: bool,
    pub package_transports: String,
    pub remote_transports: String,
    pub published_at: Option<String>,
    pub synced_at: DateTime<Utc>,
    pub current: Option<CurrentSurface>,
    pub findings_high: i64,
    pub findings_medium: i64,
    pub surface_endpoints: i64,
    pub approved_endpoints: i64,
    pub worst_drift: Drift,
    pub drifted_remote: Option<String>,
    /// The current endpoint's decision, if any repository in reach took one.
    pub decision: Option<Decision>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentSurface {
    pub id: i64,
    pub source: SurfaceSource,
    pub remote_url: String,
    pub tools_sha256: Option<String>,
    pub permissions_sha256: String,
    pub combined_sha256: String,
}

/// A ruleset as a predicate: the rows it can possibly admit. Only ever
/// narrows what is read; the gate still judges every row it lets through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameFilter {
    pub rules: Vec<AllowRule>,
}

impl NameFilter {
    pub fn allows(&self) -> impl Iterator<Item = &AllowRule> {
        self.rules.iter().filter(|r| r.effect == Effect::Allow)
    }

    /// The deny rules no longer allow rule can reopen inside: subtracting
    /// any other would remove a row the gate admits.
    pub fn subtractable_denies(&self) -> impl Iterator<Item = &AllowRule> {
        self.rules.iter().filter(move |deny| {
            deny.effect == Effect::Deny
                && !self.allows().any(|allow| {
                    allow.pattern.len() > deny.pattern.len()
                        && deny.prefix().is_some_and(|p| allow.pattern.starts_with(p))
                })
        })
    }
}

/// What one member's page reads, in `(name, version)` order.
#[derive(Debug, Clone, Default)]
pub struct PageQuery {
    pub member: i64,
    pub addressed: i64,
    pub after: Option<(String, String)>,
    pub limit: i64,
    pub include_deleted: bool,
    pub search: Option<String>,
    pub updated_since: Option<DateTime<Utc>>,
    pub latest_only: bool,
    pub version: Option<String>,
    /// Every ruleset in force as a removal, ANDed: the addressed
    /// repository's under `Hide`, and the member's floor.
    pub filters: Vec<NameFilter>,
    /// Under `Hide`: only versions whose verdict is approved and unmoved.
    pub require_approved: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    pub high_water: Option<DateTime<Utc>>,
    pub last_full_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub skipped: i64,
    pub consecutive_failures: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub id: i64,
    pub pattern: String,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRule {
    pub id: i64,
    pub rule: AllowRule,
}

/// A version whose remotes may be probed, and when each was last tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    pub version_id: i64,
    pub envelope_json: String,
    pub last_runs: Vec<ProbeRunRow>,
}

/// A skill archive to record once its bytes are placed: the pins are the
/// placement's, spent in the same transaction.
#[derive(Debug, Clone)]
pub struct NewSkill {
    pub repository: i64,
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub size: i64,
    pub description: Option<String>,
    pub allowed_tools: Option<String>,
    pub surface_sha256: String,
    pub findings: Vec<NewFinding>,
    /// Natively `high` findings in the frontmatter: the skill is never
    /// distributed while any stands.
    pub blocking: i64,
    pub published_by: Option<String>,
    pub pins: Vec<crate::ports::reclaim::PinToken>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    pub id: i64,
    pub member: i64,
    pub name: String,
    pub version: String,
    pub key: String,
    pub sha256: String,
    pub size: i64,
    pub description: Option<String>,
    pub allowed_tools: Option<String>,
    pub surface_sha256: String,
    pub findings_high: i64,
    pub findings_medium: i64,
    pub blocking: i64,
    pub published_at: DateTime<Utc>,
    /// The addressed repository's decision on this exact surface, the
    /// member's as the fallback.
    pub decision: Option<Decision>,
}

#[async_trait]
pub trait McpStore: Send + Sync {
    /// `Conflict` when the version exists, `Superseded` when a pin was
    /// revoked; neither writes anything.
    async fn publish_skill(&self, skill: &NewSkill) -> Result<i64, StoreError>;

    /// Every skill of a hosted member, newest first.
    async fn skills(&self, member: i64, addressed: i64) -> Result<Vec<SkillRow>, StoreError>;

    async fn skill(&self, member: i64, addressed: i64, name: &str, version: &str) -> Result<Option<SkillRow>, StoreError>;

    /// The row and its findings; its key is enqueued in the same
    /// transaction and returned, never deleted here.
    async fn delete_skill(&self, repository: i64, name: &str, version: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError>;

    /// Insert or update in place, never replace: the row id is what
    /// surfaces, findings and the policy writer hold. The declared surface
    /// is upserted and its findings replaced in the same transaction.
    async fn upsert_record(&self, record: &RecordWrite) -> Result<Upserted, StoreError>;

    /// A probe run, and on success its surface with its findings replaced.
    async fn record_probe(&self, run: &ProbeRun) -> Result<Option<i64>, StoreError>;

    /// A surface a runner attested, with its findings replaced.
    async fn record_attested(
        &self,
        version_id: i64,
        surface: &NewSurface,
        now: DateTime<Utc>,
    ) -> Result<i64, StoreError>;

    /// Decisions, one per endpoint, updated in place.
    async fn decide(&self, approvals: &[NewApproval]) -> Result<Vec<i64>, StoreError>;

    async fn page(&self, query: &PageQuery) -> Result<Vec<CatalogRow>, StoreError>;

    /// Newest first.
    async fn versions_of(
        &self,
        member: i64,
        addressed: i64,
        name: &str,
        include_deleted: bool,
    ) -> Result<Vec<CatalogRow>, StoreError>;

    /// `version` of `None` is the latest.
    async fn version(
        &self,
        member: i64,
        addressed: i64,
        name: &str,
        version: Option<&str>,
    ) -> Result<Option<CatalogRow>, StoreError>;

    async fn version_by_id(&self, version_id: i64, addressed: i64) -> Result<Option<CatalogRow>, StoreError>;

    /// The live surfaces of a version, current first.
    async fn surfaces(&self, version_id: i64) -> Result<Vec<SurfaceRow>, StoreError>;

    /// The findings of every live surface of a version, marked suppressed
    /// by the addressed repository's suppressions.
    async fn findings_of(&self, version_id: i64, addressed: i64) -> Result<Vec<FindingRow>, StoreError>;

    async fn approvals_of(&self, name: &str, version: &str, skill: bool) -> Result<Vec<ApprovalRow>, StoreError>;

    async fn probe_runs(&self, version_id: i64) -> Result<Vec<ProbeRunRow>, StoreError>;

    /// Latest active versions of a mirror with a remote, with their runs.
    async fn probe_targets(&self, repository: i64) -> Result<Vec<ProbeTarget>, StoreError>;

    async fn allow_rules(&self, repository: i64) -> Result<Vec<StoredRule>, StoreError>;

    /// `Conflict` when the pattern already has a rule.
    async fn add_allow_rule(
        &self,
        repository: i64,
        rule: &AllowRule,
        by: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<i64, StoreError>;

    async fn delete_allow_rule(&self, repository: i64, id: i64) -> Result<(), StoreError>;

    async fn suppressions(&self, repository: i64) -> Result<Vec<Suppression>, StoreError>;

    /// The repository's sparse counts are rebuilt in the same transaction.
    async fn add_suppression(
        &self,
        repository: i64,
        pattern: &str,
        tool: &str,
        by: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<i64, StoreError>;

    async fn delete_suppression(&self, repository: i64, id: i64, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Allow rules and suppressions a config file declares, each written
    /// once and never overwritten.
    async fn seed(
        &self,
        repository: i64,
        rules: &[AllowRule],
        suppressions: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn sync_state(&self, repository: i64) -> Result<SyncState, StoreError>;

    async fn save_sync_state(&self, repository: i64, state: &SyncState) -> Result<(), StoreError>;

    /// Rows a human wrote: hosted records and skills.
    async fn hosted_count(&self, repository: i64) -> Result<i64, StoreError>;

    /// Every synced row of a mirror and what hangs off it.
    async fn purge(&self, repository: i64) -> Result<u64, StoreError>;
}
