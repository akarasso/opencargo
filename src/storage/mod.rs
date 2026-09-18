use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncRead;

mod filesystem;
// The re-export is what makes clippy.toml's `opencargo::storage::FilesystemStorage`
// resolve; naming the concrete backend is this module's job and nobody else's.
#[allow(clippy::disallowed_types)]
pub use filesystem::FilesystemStorage;

/// How a storage backend refuses: the port's own vocabulary, so nothing above
/// it has to know which backend answered, and nothing in here has to know that
/// HTTP exists.
///
/// `InvalidPath` is not a variant for symmetry. "This key is not addressable"
/// is a fact every backend has — the filesystem's traversal guard and
/// `s3.md`'s `validate_key` are the same refusal over different key spaces —
/// and it is the one that must keep its 400; folded into `Other` it would
/// become a 500.
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

#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn get(&self, path: &str) -> Result<Bytes, StorageError>;
    async fn put(&self, path: &str, data: Bytes) -> Result<(), StorageError>;
    /// Append bytes to a file (creating it if absent) and return the new total
    /// size. Used by chunked uploads to avoid re-reading and rewriting the whole
    /// blob on every chunk (turns an O(N²) accumulation into O(N)).
    async fn append(&self, path: &str, data: Bytes) -> Result<u64, StorageError>;
    async fn delete(&self, path: &str) -> Result<(), StorageError>;
    /// Recursively delete everything under a path prefix (e.g. a repo's proxy
    /// cache directory). A no-op if nothing exists at the prefix.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), StorageError>;
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;
    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError>;
    /// Open once and hand out the reader with its length, so a later rename of
    /// the path leaves the reader on the old, complete inode.
    async fn read_stream(
        &self,
        path: &str,
    ) -> Result<(u64, Pin<Box<dyn AsyncRead + Send>>), StorageError>;
    /// Remove every `*.part-*` file under `prefix` whose mtime is older than
    /// `older_than`; returns how many were removed.
    async fn remove_stale_parts(
        &self,
        prefix: &str,
        older_than: Duration,
    ) -> Result<u64, StorageError>;
    /// Validate a key and give back the local name the backend writes it under.
    /// Only the part uploader needs it, and only until `s3.md` replaces it with
    /// `writer`/`reserve`/`commit`.
    fn resolve(&self, path: &str) -> Result<PathBuf, StorageError>;
}

/// The one place the concrete backend is named outside its own module: the
/// composition root asks for storage, not for a filesystem.
#[allow(clippy::disallowed_types)]
pub fn filesystem(base_path: impl Into<PathBuf>) -> Arc<dyn StorageBackend> {
    Arc::new(FilesystemStorage::new(base_path))
}
