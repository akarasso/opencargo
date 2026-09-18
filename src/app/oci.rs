//! The OCI push half: what a manifest write, a manifest delete, a completed
//! upload and a blob delete each order.
//!
//! Every one of them touches both the object store and the database, and the
//! order is the whole content of these four use cases: the object lands
//! before the rows that claim it, and the rows go before the objects they
//! stop claiming. A manifest delete is the one that cannot be written the
//! other way round — the blobs it orphans are not known until its link rows
//! are gone (§2.4, rule B), so the transaction reports them and the objects
//! follow the commit.

use std::sync::Arc;

use bytes::Bytes;
use tracing::warn;

use crate::error::{AppError, StoreError};
use crate::ports::oci::{NewBlob, NewManifest, OciStore};
use crate::storage::{StorageBackend, StorageError};

/// How an OCI write refuses. `NotFound` and `Referenced` carry no message:
/// the wording names the image and the repository, which the protocol
/// adapter holds and this layer does not.
#[derive(Debug, thiserror::Error)]
pub enum OciWriteError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error("no such object")]
    NotFound,

    /// The blob is still a layer of this many manifests.
    #[error("still referenced by {0} manifest(s)")]
    Referenced(i64),
}

impl From<OciWriteError> for AppError {
    fn from(err: OciWriteError) -> Self {
        match err {
            OciWriteError::Store(err) => err.into(),
            OciWriteError::Storage(err) => err.into(),
            OciWriteError::NotFound => AppError::NotFound("not found".to_string()),
            OciWriteError::Referenced(n) => {
                AppError::Conflict(format!("still referenced by {n} manifest(s)"))
            }
        }
    }
}

/// One pushed manifest: its bytes, where they go, and what points at it.
pub struct PushedManifest<'a> {
    pub repository: i64,
    pub name: &'a str,
    pub digest: &'a str,
    pub content_type: &'a str,
    /// The blobs the manifest references, in the order it lists them.
    pub blobs: Vec<String>,
    /// The tag the push named, absent when the reference was already a digest.
    pub tag: Option<&'a str>,
    pub path: &'a str,
    pub body: Bytes,
}

/// The manifest a delete names, already resolved to a digest.
pub struct ManifestTarget<'a> {
    pub repository: i64,
    pub name: &'a str,
    pub digest: &'a str,
    pub path: &'a str,
}

/// The file first, then the rows that claim it: a row pointing at bytes that
/// are not there would serve a 500 on every pull.
pub struct PutManifest {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
}

impl PutManifest {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { oci, storage }
    }

    pub async fn run(&self, pushed: PushedManifest<'_>) -> Result<(), OciWriteError> {
        self.storage.put(pushed.path, pushed.body.clone()).await?;
        self.oci
            .put_manifest(NewManifest {
                repository: pushed.repository,
                name: pushed.name,
                digest: pushed.digest,
                content_type: pushed.content_type,
                size: pushed.body.len() as i64,
                blobs: &pushed.blobs,
                tag: pushed.tag,
            })
            .await?;
        Ok(())
    }
}

/// The rows in one transaction, then the objects that transaction orphaned.
pub struct DeleteManifest {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
}

impl DeleteManifest {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { oci, storage }
    }

    /// `blob_key` spells an orphaned digest the way its repository stores it;
    /// the orphan set does not exist until the commit, so the layout has to
    /// come in rather than the keys.
    pub async fn run<K>(&self, target: ManifestTarget<'_>, blob_key: K) -> Result<(), OciWriteError>
    where
        K: Fn(&str) -> String,
    {
        let orphaned = self
            .oci
            .delete_manifest(target.repository, target.name, target.digest)
            .await?
            .ok_or(OciWriteError::NotFound)?;

        // Logged before the objects go, because a crash in the middle leaves
        // unreferenced files and nothing else records what was meant to.
        if !orphaned.blob_digests.is_empty() {
            warn!(
                image = %target.name,
                manifest = %target.digest,
                blobs = ?orphaned.blob_digests,
                "OCI manifest deleted; removing the blobs it orphaned"
            );
        }
        for digest in &orphaned.blob_digests {
            let _ = self.storage.delete(&blob_key(digest)).await;
        }
        let _ = self.storage.delete(target.path).await;
        Ok(())
    }
}

/// The assembled blob, then the row and the ledger entry, then the scratch.
pub struct CompleteUpload {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
}

/// An upload's last chunk: the bytes, where the blob lands, and the scratch
/// file they were assembled in.
pub struct AssembledBlob<'a> {
    pub upload: &'a str,
    pub repository: i64,
    pub digest: &'a str,
    pub content_type: &'a str,
    pub path: &'a str,
    pub scratch: &'a str,
    pub bytes: Bytes,
}

impl CompleteUpload {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { oci, storage }
    }

    pub async fn run(&self, blob: AssembledBlob<'_>) -> Result<(), OciWriteError> {
        self.storage.put(blob.path, blob.bytes.clone()).await?;
        self.oci
            .complete_upload(
                blob.upload,
                NewBlob {
                    repository: blob.repository,
                    digest: blob.digest,
                    size: blob.bytes.len() as i64,
                    content_type: blob.content_type,
                },
            )
            .await?;
        let _ = self.storage.delete(blob.scratch).await;
        Ok(())
    }
}

/// A blob a client asks to forget: refused while a manifest still lists it,
/// then the row, then the object.
pub struct DeleteBlob {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
}

impl DeleteBlob {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { oci, storage }
    }

    pub async fn run(
        &self,
        repository: i64,
        digest: &str,
        path: &str,
    ) -> Result<(), OciWriteError> {
        // A blob still referenced by a manifest would break a live image.
        let referenced = self.oci.blob_references(repository, digest).await?;
        if referenced > 0 {
            return Err(OciWriteError::Referenced(referenced));
        }
        if !self.oci.delete_blob(repository, digest).await? {
            return Err(OciWriteError::NotFound);
        }
        let _ = self.storage.delete(path).await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "oci_tests.rs"]
mod tests;
