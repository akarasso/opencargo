//! The object storage port (S3 v5 §2): whole-object reads and atomic
//! writes, listing, copies, and the bounds a caller derives liveness from.
//! No operation names a filesystem path, a bucket or a URL.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use tokio::io::AsyncRead;

pub mod keys;

#[cfg(test)]
pub mod contract;

/// How a storage backend refuses: the port's own vocabulary.
///
/// `InvalidPath` is decided before any I/O and keeps its 400; `Unavailable`
/// is every other fault and carries no text, the detail being logged by the
/// adapter where it maps the fault; `Other` is an adapter invariant breach.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("file not found")]
    NotFound,

    #[error("{0}")]
    InvalidPath(String),

    #[error("the storage backend is unavailable, try again")]
    Unavailable,

    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl From<std::io::Error> for StorageError {
    fn from(err: std::io::Error) -> Self {
        StorageError::Other(Box::new(err))
    }
}

/// A committed object as `list`, `head` and `stat` describe it. The key is
/// logical: no backend prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// Never earlier than the instant the object became visible at `key`.
    pub last_modified: DateTime<Utc>,
}

pub type ObjectBody = Pin<Box<dyn AsyncRead + Send>>;

pub struct ReadStream {
    pub total: u64,
    pub body: ObjectBody,
}

pub type ObjectList = Pin<Box<dyn Stream<Item = Result<ObjectMeta, StorageError>> + Send>>;

/// A streamed create-or-replace. Nothing is visible until `commit`, and a
/// writer dropped before it leaves no object.
#[async_trait]
pub trait ObjectWriter: Send {
    /// The only await that may queue on a backend-wide resource. No caller
    /// wraps it, or `commit`, in a deadline.
    async fn reserve(&mut self, next_len: usize) -> Result<(), StorageError>;

    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError>;

    /// Atomic and uncancellable; answers the committed size.
    async fn commit(self: Box<Self>) -> Result<u64, StorageError>;
}

/// What a protocol adapter may advertise, and the backend's own bounds, from
/// which leases and claims derive their length for liveness only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPlan {
    pub max_object_bytes: u64,
    pub min_chunk_bytes: u64,
    pub completion_bound: Duration,
    pub delete_bound: Duration,
    pub delete_batch: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckStep {
    pub operation: &'static str,
    pub outcome: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckReport {
    pub steps: Vec<CheckStep>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.outcome.is_ok())
    }
}

/// Whether the store keeps noncurrent versions of an overwritten or deleted
/// key, which is what `storage verify --repair` puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versioning {
    Kept,
    NotKept,
    /// The store may keep them; this adapter cannot tell.
    Unknown,
}

/// An opaque, declared name for a store: never an endpoint, bucket or path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoreIdentity(pub String);

#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Bytes, StorageError>;

    /// `writer` + one reserved write + `commit`.
    async fn put(&self, key: &str, data: Bytes) -> Result<(), StorageError> {
        let mut writer = self.writer(key).await?;
        writer.reserve(data.len()).await?;
        writer.write(data).await?;
        writer.commit().await?;
        Ok(())
    }

    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError>;

    /// The whole object; a mid-body failure is an error, never mixed bytes.
    async fn read_stream(&self, key: &str) -> Result<ReadStream, StorageError>;

    /// Server-side copy; `Ok` only once an uncached size check of `to` holds.
    async fn copy_object(&self, from: &str, to: &str) -> Result<(), StorageError>;

    /// A copy whose source need not survive; idempotent on the target.
    async fn relocate(&self, from: &str, to: &str) -> Result<(), StorageError>;

    /// Existence and size, possibly from a cache: only where a read path
    /// heals a stale positive.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError>;

    /// Existence and size, never cached.
    async fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError>;

    /// Absent is `Ok`.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// All or an error, never a partial `Ok`.
    async fn delete_batch(&self, keys: &[String]) -> Result<(), StorageError>;

    /// Recursive and segment-wise: the key equal to `prefix` and everything
    /// under `prefix/`, committed objects only, reserved segments excluded.
    fn list(&self, prefix: &str) -> ObjectList;

    /// Reclaims in-flight writer residue older than `older_than`.
    async fn sweep_abandoned(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StorageError>;

    async fn probe(&self) -> Result<(), StorageError>;

    /// Read at startup and before a repair; never assumed.
    async fn versioning(&self) -> Result<Versioning, StorageError> {
        Ok(Versioning::Unknown)
    }

    /// Puts the last noncurrent version of `key` back as the current one.
    /// `Ok(false)` when the store has none to put back.
    async fn restore_last_version(&self, _key: &str) -> Result<bool, StorageError> {
        Ok(false)
    }

    fn upload_plan(&self) -> UploadPlan;

    /// What a trait call may name here: a backend with a prefix of its own
    /// leaves less than `MAX_KEY_BYTES` for the key, and one whose segments
    /// are file names bounds each of them.
    fn key_budget(&self) -> keys::KeyBudget {
        keys::KeyBudget::UNSEGMENTED
    }

    async fn self_check(&self) -> CheckReport;

    fn identity(&self) -> StoreIdentity;
}
