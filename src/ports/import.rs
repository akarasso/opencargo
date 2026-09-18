//! A migration import: a source registry enumerated into items, a sink per
//! format that copies one item into this registry over its own protocols,
//! and the journal that makes the whole run resumable.
//!
//! Nothing here names a vendor: the sources are adapters, and an `Origin` is
//! coordinates only, never a URL a source handed over, so no signature and
//! no credential can reach the journal through it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use crate::domain::import::{GapKind, ItemStatus, SourceFormat};
use crate::domain::Format;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coord {
    /// The source repository, project or namespace.
    pub repo: String,
    pub name: String,
    /// The tag, for OCI.
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digests {
    pub sha256: Option<String>,
    pub sha1: Option<String>,
    /// An SRI string, `sha512-...` for npm.
    pub integrity: Option<String>,
    pub md5: Option<String>,
}

impl Digests {
    pub fn is_empty(&self) -> bool {
        self.sha256.is_none() && self.sha1.is_none() && self.integrity.is_none()
    }
}

/// Where an item's bytes are, as coordinates the adapter re-derives a
/// request from on every copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Origin {
    /// A file in a repository manager, at `{endpoint}{repo}/{path}`.
    Asset { endpoint: String, repo: String, path: String },
    /// An npm registry endpoint: its packument, then the version's tarball.
    Npm { registry: String, package: String },
    /// A sparse cargo index: the version's line, then its `.crate`.
    Cargo { index: String, name: String, version: String },
    /// A GOPROXY endpoint.
    Go { proxy: String, module: String, version: String },
    /// A distribution-protocol registry.
    Oci { registry: String, image: String, reference: String },
}

impl Origin {
    /// Every string the origin holds, for the persist-time redaction check.
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Origin::Asset { endpoint, repo, path } => vec![endpoint, repo, path],
            Origin::Npm { registry, package } => vec![registry, package],
            Origin::Cargo { index, name, version } => vec![index, name, version],
            Origin::Go { proxy, module, version } => vec![proxy, module, version],
            Origin::Oci { registry, image, reference } => vec![registry, image, reference],
        }
    }
}

/// Facts about a package, not a version: sealed once its versions landed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PkgExtra {
    pub dist_tags: Vec<(String, String)>,
    pub labels: Vec<String>,
}

/// Facts about one version the publish body cannot carry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionExtra {
    pub yanked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The idempotency key, stable across runs.
    pub source_ref: String,
    pub format: SourceFormat,
    pub coord: Coord,
    pub published_at: Option<DateTime<Utc>>,
    pub size: Option<u64>,
    pub want: Digests,
    pub origin: Origin,
    pub pkg: PkgExtra,
    pub extra: VersionExtra,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Gap {
    pub kind: GapKind,
    pub source_ref: String,
    pub detail: String,
}

impl Gap {
    pub fn new(kind: GapKind, source_ref: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { kind, source_ref: source_ref.into(), detail: detail.into() }
    }
}

/// An item the plan kept, with where it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planned {
    pub item: Item,
    pub target_repo: String,
    pub target_name: String,
    pub target_format: Format,
}

/// What the source filter narrows before an item is ever emitted; the plan
/// applies the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceFilter {
    pub source_repos: Vec<String>,
    pub include_proxy_caches: bool,
    pub max_pages: u32,
}

pub type Cursor = Option<String>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    pub product: String,
    pub version: Option<String>,
    pub authenticated_as: Option<String>,
    pub capabilities: Vec<String>,
}

/// One right a source grants one identity on one of its repositories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub name: String,
    /// `user`, `group`, `robot`, `token`: only users map.
    pub kind: String,
    pub repo: String,
    pub read: bool,
    pub publish: bool,
}

/// The batch a `discover` call fills; the run loop drains it into the
/// journal in one transaction, so a source never touches the journal.
pub trait Discovered: Send {
    fn item(&mut self, it: Item);
    fn gap(&mut self, g: Gap);
}

#[derive(Debug, Default)]
pub struct Batch {
    pub items: Vec<Item>,
    pub gaps: Vec<Gap>,
}

impl Discovered for Batch {
    fn item(&mut self, it: Item) {
        self.items.push(it);
    }
    fn gap(&mut self, g: Gap) {
        self.gaps.push(g);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// Credentials refused, or none where some are required.
    #[error("source refused the credentials: {0}")]
    Auth(String),
    #[error("source is unreachable or answered unexpectedly: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Refused(String),
}

#[async_trait]
pub trait Source: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Read-only: capabilities are observed, never assumed.
    async fn probe(&self) -> Result<Probe, SourceError>;

    /// The units the journal keeps one cursor for, stable across runs.
    async fn streams(&self, f: &SourceFilter) -> Result<Vec<String>, SourceError>;

    /// One batch of one stream; `true` when the stream is done.
    async fn discover(
        &self,
        f: &SourceFilter,
        stream: &str,
        at: Cursor,
        out: &mut dyn Discovered,
    ) -> Result<(Cursor, bool), SourceError>;

    async fn principals(&self) -> Result<Vec<Principal>, SourceError> {
        Ok(Vec::new())
    }
}

/// What the target holds at an item's coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Absent,
    /// There, with no checksum the protocol exposes to compare.
    Present,
    Same,
    Different(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Copied {
    pub bytes: u64,
    pub sha256: String,
    /// `Retagged` and the like: an annotation of a success.
    pub note: Option<String>,
    /// Facts the copy learned that the target cannot carry.
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CopyError {
    #[error("{0}")]
    Permanent(String),
    #[error("{0}")]
    Transient(String),
    #[error("{size} bytes over the {limit}-byte limit")]
    TooLarge { size: u64, limit: u64 },
    /// The target's own gate refused the content.
    #[error("{0}")]
    Refused(String),
    /// An immutable coordinate already holds different content.
    #[error("{0}")]
    Conflict(String),
    /// The target stayed unavailable through every cooldown: stop the run.
    #[error("{0}")]
    Stalled(String),
}

#[async_trait]
pub trait Sink: Send + Sync {
    fn format(&self) -> Format;

    /// A read on the target, before a byte moves.
    async fn present(&self, it: &Planned) -> Result<Presence, CopyError>;

    async fn copy(&self, it: &Planned) -> Result<Copied, CopyError>;

    /// Once per package, after all of its versions are terminal; `versions`
    /// lists the ones that landed.
    async fn seal(
        &self,
        repo: &str,
        name: &str,
        pkg: &PkgExtra,
        versions: &[(String, VersionExtra)],
    ) -> Result<Vec<Gap>, CopyError>;
}

/// One repository as the importing token sees it on the target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TargetRepo {
    pub repository: String,
    #[serde(rename = "type")]
    pub repo_type: String,
    pub format: String,
    pub visibility: String,
    pub can_read: bool,
    pub can_write: bool,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TargetIdentity {
    pub username: String,
    pub role: String,
    pub permissions: Vec<TargetRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("the target does not answer {0}")]
    Unsupported(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Unavailable(String),
}

/// The target's admin surface: who the importing token is, and what an admin
/// token may create for it.
#[async_trait]
pub trait TargetAdmin: Send + Sync {
    async fn identity(&self) -> Result<TargetIdentity, TargetError>;
    async fn create_repository(&self, name: &str, format: Format) -> Result<(), TargetError>;
    async fn user_exists(&self, username: &str) -> Result<bool, TargetError>;
    async fn grant(&self, username: &str, repo: &str, read: bool, write: bool) -> Result<(), TargetError>;
    async fn create_user(&self, username: &str) -> Result<(), TargetError>;
}

/// Which worker pool claims an item: blobs are memory-heavy on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Blob,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHeader {
    pub source: String,
    pub source_url: String,
    pub target_url: String,
    pub opts_json: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub phase: String,
    pub owner: Option<String>,
}

/// An item as the journal holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journaled {
    pub planned: Planned,
    pub status: ItemStatus,
    pub attempts: u32,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    /// `kind: message` for a failure, so the report can re-derive its gap.
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: ItemStatus,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub error: Option<String>,
    pub note: Option<String>,
    /// Facts the copy learned, replacing the ones an earlier copy stored.
    pub gaps: Vec<Gap>,
}

/// A package whose versions are all terminal and which was never sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsealed {
    pub target_repo: String,
    pub name: String,
    pub format: Format,
    pub extra: PkgExtra,
    pub landed: Vec<(String, VersionExtra)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("the state file is held by {0}")]
    Busy(String),
    /// Another owner took the journal over; this run must stop writing.
    #[error("the state file was taken over by {0}")]
    Lost(String),
    #[error("{0}")]
    Incompatible(String),
    /// An origin carried a query string or credentials.
    #[error("{0}")]
    Unredacted(String),
    #[error("state file: {0}")]
    Io(String),
}

#[async_trait]
pub trait ImportJournal: Send + Sync {
    async fn header(&self) -> Result<Option<RunHeader>, JournalError>;

    /// Claims the journal for `owner`, refusing a live other one, and
    /// requeues every failure and every item a dead owner left running. A
    /// fresh run also rewinds every cursor.
    async fn begin(
        &self,
        header: &RunHeader,
        owner: &str,
        fresh: bool,
        now: DateTime<Utc>,
    ) -> Result<(), JournalError>;

    /// Once `begin` succeeded, every write through this handle, including
    /// `heartbeat` and `finish`, is refused with `Lost` if another owner has
    /// since taken the journal over.
    async fn heartbeat(&self, owner: &str, now: DateTime<Utc>) -> Result<(), JournalError>;

    async fn finish(&self, phase: &str, now: DateTime<Utc>) -> Result<(), JournalError>;

    /// Records the streams, keeping the cursors of known ones.
    async fn streams(&self, names: &[String]) -> Result<(), JournalError>;

    async fn cursor(&self, stream: &str) -> Result<(Cursor, bool), JournalError>;

    /// One discovery batch, atomically: its planned items, its gaps and the
    /// stream's new cursor. The stream's earlier gaps go when it restarts.
    async fn record(
        &self,
        stream: &str,
        restarted: bool,
        planned: &[Planned],
        gaps: &[Gap],
        cursor: &Cursor,
        done: bool,
    ) -> Result<(), JournalError>;

    /// Gaps no stream owns, replaced as a set.
    async fn replace_run_gaps(&self, gaps: &[Gap]) -> Result<(), JournalError>;

    /// Removes the unfinished items that share a target coordinate with
    /// another source coordinate, and records a gap naming each group.
    async fn take_collisions(&self) -> Result<Vec<Gap>, JournalError>;

    /// The target repositories the plan routes to, with their format.
    async fn targets(&self) -> Result<Vec<(String, Format)>, JournalError>;

    async fn claim(&self, lane: Lane, now: DateTime<Utc>) -> Result<Option<Journaled>, JournalError>;

    async fn complete(&self, source_ref: &str, outcome: &Outcome) -> Result<(), JournalError>;

    async fn unsealed(&self) -> Result<Vec<Unsealed>, JournalError>;

    async fn sealed(&self, target_repo: &str, name: &str, gaps: &[Gap]) -> Result<(), JournalError>;

    async fn items(&self) -> Result<Vec<Journaled>, JournalError>;

    /// Every stored gap once, with how many times its fact was recorded.
    async fn gaps(&self) -> Result<Vec<(Gap, u64)>, JournalError>;
}

/// A URL as it may be stored or printed: no credentials, no query string,
/// where pre-signed downloads carry their signature.
pub fn redact(url: &url::Url) -> String {
    let mut out = url.clone();
    let _ = out.set_username("");
    let _ = out.set_password(None);
    out.set_query(None);
    out.set_fragment(None);
    out.to_string()
}

/// A string that redacting would change: a URL carrying credentials, a
/// query or a fragment. Anything that is not a URL is coordinates.
pub fn needs_redaction(s: &str) -> bool {
    match url::Url::parse(s) {
        Ok(u) if u.has_host() => redact(&u) != u.as_str(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_drops_userinfo_and_the_whole_query() {
        let u = url::Url::parse(
            "https://admin:hunter2@nexus.internal/repository/r/x.tgz?X-Amz-Signature=abc#f",
        )
        .unwrap();
        assert_eq!(redact(&u), "https://nexus.internal/repository/r/x.tgz");
        assert!(needs_redaction(u.as_str()));
        assert!(!needs_redaction("https://nexus.internal/repository/r/"));
        assert!(!needs_redaction("@acme/utils"));
        assert!(!needs_redaction("github.com/!burnt!sushi/toml"));
    }
}
