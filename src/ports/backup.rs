//! What a backup needs of the database: a consistent copy of a live one, a
//! checkpoint afterwards, and the file-level half of a restore.

use std::path::Path;

use async_trait::async_trait;

use crate::error::StoreError;

/// The outcome of a WAL checkpoint: `busy` when a reader held it off, which
/// is reported, never a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub busy: bool,
    pub log_pages: i64,
    pub checkpointed_pages: i64,
}

#[async_trait]
pub trait DatabaseBackup: Send + Sync {
    /// A consistent copy of the live database at `dest`, which must not exist.
    async fn snapshot_into(&self, dest: &Path) -> Result<(), StoreError>;

    /// Truncates the write-ahead log the copy held open.
    async fn checkpoint(&self) -> Result<Checkpoint, StoreError>;

    /// The live database's size in bytes.
    async fn size(&self) -> Result<u64, StoreError>;
}

/// A database as files, while nothing holds it open.
#[async_trait]
pub trait DatabaseFiles: Send + Sync {
    /// `Err` names the first integrity problem of the database file at `file`.
    async fn verify(&self, file: &Path) -> Result<(), String>;

    /// Puts `snapshot` in place of the database at `db_path`, dropping the
    /// old database's side files first so their pages are never replayed
    /// onto the restored file.
    async fn replace(&self, db_path: &Path, snapshot: &Path) -> Result<(), StoreError>;
}
