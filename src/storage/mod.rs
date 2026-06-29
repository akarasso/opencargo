use async_trait::async_trait;
use bytes::Bytes;

use crate::error::AppError;

mod filesystem;
pub use filesystem::FilesystemStorage;

#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn get(&self, path: &str) -> Result<Bytes, AppError>;
    async fn put(&self, path: &str, data: Bytes) -> Result<(), AppError>;
    /// Append bytes to a file (creating it if absent) and return the new total
    /// size. Used by chunked uploads to avoid re-reading and rewriting the whole
    /// blob on every chunk (turns an O(N²) accumulation into O(N)).
    async fn append(&self, path: &str, data: Bytes) -> Result<u64, AppError>;
    async fn delete(&self, path: &str) -> Result<(), AppError>;
    /// Recursively delete everything under a path prefix (e.g. a repo's proxy
    /// cache directory). A no-op if nothing exists at the prefix.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError>;
    async fn exists(&self, path: &str) -> Result<bool, AppError>;
}
