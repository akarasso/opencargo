//! The OCI push half: a manifest write, a manifest delete, a blob delete, and
//! an upload session's chunks and completion.
//!
//! Shared keys (blobs, manifests) are placed only through `place_shared` and
//! released only by enqueue: a delete here removes rows and deletes nothing.
//! Upload segments are private to their session: a chunk that loses its
//! compare-and-set deletes its own segment, and the completion that won
//! `finish_upload` deletes the session's (A1 C5bis).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::Digest;
use tokio::io::AsyncReadExt;
use tracing::warn;

use crate::app::place::{Entry, PlaceError, Placer, Source};
use crate::domain::layout;
use crate::error::{AppError, StoreError};
use crate::ports::oci::{
    BeginComplete, Finished, LeaseToken, NewBlob, NewManifest, OciStore, Segment, SegmentClaim,
};
use crate::storage::{StorageBackend, StorageError};

/// Segments one session may hold before a chunk is refused.
pub const MAX_SEGMENTS: u32 = 10_000;

/// How an OCI write refuses. The protocol adapter words them: it holds the
/// image and the repository names, this layer does not.
#[derive(Debug, thiserror::Error)]
pub enum OciWriteError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error(transparent)]
    Place(#[from] PlaceError),

    #[error("no such object")]
    NotFound,

    /// The blob is still a layer of a manifest.
    #[error("still referenced by a manifest")]
    Referenced,

    /// A manifest lists a blob this repository does not hold.
    #[error("the manifest references an unknown blob")]
    BlobUnknown,

    /// A chunk that does not start where the session stands.
    #[error("the chunk does not start at {received}")]
    OutOfRange { received: u64 },

    #[error("too many chunks in one upload")]
    TooManySegments,

    #[error("digest mismatch: computed {computed}")]
    DigestMismatch { computed: String },

    #[error("no blob data provided")]
    Empty,

    /// Another completion of the same session holds its lease.
    #[error("the upload is being completed, try again")]
    Completing,
}

impl From<OciWriteError> for AppError {
    fn from(err: OciWriteError) -> Self {
        match err {
            OciWriteError::Store(err) => err.into(),
            OciWriteError::Storage(err) => err.into(),
            OciWriteError::Place(err) => err.into(),
            OciWriteError::NotFound => AppError::NotFound("not found".to_string()),
            OciWriteError::Referenced => AppError::Conflict(err.to_string()),
            OciWriteError::BlobUnknown
            | OciWriteError::DigestMismatch { .. }
            | OciWriteError::Empty
            | OciWriteError::OutOfRange { .. }
            | OciWriteError::TooManySegments => AppError::BadRequest(err.to_string()),
            OciWriteError::Completing => AppError::ServiceUnavailable(err.to_string()),
        }
    }
}

/// One pushed manifest: its bytes and what it points at.
pub struct PushedManifest<'a> {
    pub repository: i64,
    pub repo_prefix: &'a str,
    pub name: &'a str,
    pub digest: &'a str,
    pub content_type: &'a str,
    /// The config and layers, in the order the manifest lists them.
    pub blobs: Vec<String>,
    /// The child manifests of an index.
    pub children: Vec<String>,
    pub tag: Option<&'a str>,
    pub body: Bytes,
}

fn hex(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// The manifest's bytes and a pin on every blob it lists, in one placement;
/// the rows commit only if every blob has one and no pin was revoked.
pub struct PutManifest {
    oci: Arc<dyn OciStore>,
    placer: Arc<Placer>,
}

impl PutManifest {
    pub fn new(oci: Arc<dyn OciStore>, placer: Arc<Placer>) -> Self {
        Self { oci, placer }
    }

    pub async fn run(
        &self,
        pushed: PushedManifest<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), OciWriteError> {
        let mut blobs = pushed.blobs.clone();
        blobs.sort();
        blobs.dedup();
        let mut entries = vec![Entry {
            logical_key: layout::oci_manifest_key(pushed.repo_prefix, pushed.name, hex(pushed.digest)),
            source: Source::Bytes(pushed.body.clone()),
        }];
        entries.extend(blobs.iter().map(|digest| Entry {
            logical_key: layout::oci_blob_key(pushed.repo_prefix, hex(digest)),
            source: Source::Existing,
        }));
        let oci = &self.oci;
        let pushed = &pushed;
        let blobs = &blobs;
        let placed = self
            .placer
            .place_shared(
                pushed.repo_prefix,
                &entries,
                |tokens| async move {
                    let pinned: Vec<(String, _)> =
                        blobs.iter().cloned().zip(tokens[1..].iter().cloned()).collect();
                    oci.put_manifest(
                        NewManifest {
                            repository: pushed.repository,
                            name: pushed.name,
                            digest: pushed.digest,
                            content_type: pushed.content_type,
                            size: pushed.body.len() as i64,
                            pin: &tokens[0],
                            blobs: &pinned,
                            children: &pushed.children,
                            tag: pushed.tag,
                        },
                        now,
                    )
                    .await
                },
                now,
            )
            .await;
        match placed {
            Ok(()) => Ok(()),
            Err(PlaceError::Refused(StoreError::NotFound)) => Err(OciWriteError::BlobUnknown),
            Err(PlaceError::Retired) => Err(OciWriteError::NotFound),
            Err(e) => Err(e.into()),
        }
    }
}

/// The manifest to delete, already resolved to a digest.
pub struct ManifestTarget<'a> {
    pub repository: i64,
    pub name: &'a str,
    pub digest: &'a str,
}

/// The rows in one transaction, which enqueues every key it released: the
/// request deletes nothing (M2).
pub struct DeleteManifest {
    oci: Arc<dyn OciStore>,
}

impl DeleteManifest {
    pub fn new(oci: Arc<dyn OciStore>) -> Self {
        Self { oci }
    }

    pub async fn run(&self, target: ManifestTarget<'_>, now: DateTime<Utc>) -> Result<(), OciWriteError> {
        let orphaned = self
            .oci
            .delete_manifest(target.repository, target.name, target.digest, now)
            .await?
            .ok_or(OciWriteError::NotFound)?;
        if !orphaned.blob_digests.is_empty() {
            tracing::info!(
                image = %target.name,
                manifest = %target.digest,
                blobs = ?orphaned.blob_digests,
                "OCI manifest deleted; its orphaned blobs are queued for reclamation"
            );
        }
        Ok(())
    }
}

/// A blob a client asks to forget: refused while a manifest lists it, then
/// the row, whose key is enqueued with it.
pub struct DeleteBlob {
    oci: Arc<dyn OciStore>,
}

impl DeleteBlob {
    pub fn new(oci: Arc<dyn OciStore>) -> Self {
        Self { oci }
    }

    pub async fn run(&self, repository: i64, digest: &str, now: DateTime<Utc>) -> Result<(), OciWriteError> {
        match self.oci.delete_blob(repository, digest, now).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(OciWriteError::NotFound),
            Err(StoreError::Conflict) => Err(OciWriteError::Referenced),
            Err(e) => Err(e.into()),
        }
    }
}

/// A chunk: its bytes into a fresh segment of the session, then the
/// compare-and-set that makes it part of the upload. The loser deletes the
/// segment it wrote, which nothing else can reach.
pub struct AppendChunk {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
}

impl AppendChunk {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { oci, storage }
    }

    /// Answers the bytes received so far.
    pub async fn run(
        &self,
        id: &str,
        repository: i64,
        start: Option<u64>,
        body: Bytes,
        now: DateTime<Utc>,
    ) -> Result<u64, OciWriteError> {
        let session = self
            .oci
            .upload(id)
            .await?
            .filter(|s| s.repository == repository)
            .ok_or(OciWriteError::NotFound)?;
        if start.is_some_and(|s| s != session.received) {
            return Err(OciWriteError::OutOfRange {
                received: session.received,
            });
        }
        if body.is_empty() {
            return Ok(session.received);
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let segment = Segment {
            start: session.received,
            len: body.len() as u64,
            key: layout::segment_key(&session.prefix, session.received, &nonce),
        };
        self.storage.put(&segment.key, body).await?;
        match self.oci.claim_segment(id, &segment, MAX_SEGMENTS, now).await {
            Ok(SegmentClaim::Won) => Ok(segment.start + segment.len),
            Ok(lost) => {
                let _ = self.storage.delete(&segment.key).await;
                if lost == SegmentClaim::TooManySegments {
                    return Err(OciWriteError::TooManySegments);
                }
                let received = self.oci.upload(id).await?.map_or(0, |s| s.received);
                Err(OciWriteError::OutOfRange { received })
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// An upload's last request, as the protocol adapter hands it over.
pub struct Completion<'a> {
    pub upload: &'a str,
    pub repository: i64,
    pub repo_prefix: &'a str,
    pub digest: &'a str,
    pub content_type: &'a str,
    pub body: Bytes,
}

/// The lease, the digest over the segments, the placement fenced by the
/// lease and the pin, and only for the winner the segments' deletion.
pub struct CompleteUpload {
    oci: Arc<dyn OciStore>,
    storage: Arc<dyn StorageBackend>,
    placer: Arc<Placer>,
}

impl CompleteUpload {
    pub fn new(oci: Arc<dyn OciStore>, storage: Arc<dyn StorageBackend>, placer: Arc<Placer>) -> Self {
        Self {
            oci,
            storage,
            placer,
        }
    }

    /// Liveness only: how long a dead completer keeps the session.
    fn lease_ttl(&self) -> Duration {
        self.storage.upload_plan().completion_bound * 2 + Duration::from_secs(60)
    }

    pub async fn run(&self, done: Completion<'_>, now: DateTime<Utc>) -> Result<(), OciWriteError> {
        if !done.body.is_empty() {
            AppendChunk::new(self.oci.clone(), self.storage.clone())
                .run(done.upload, done.repository, None, done.body.clone(), now)
                .await?;
        }
        self.oci
            .upload(done.upload)
            .await?
            .filter(|s| s.repository == done.repository)
            .ok_or(OciWriteError::NotFound)?;
        let lease = match self.oci.begin_complete(done.upload, now, self.lease_ttl()).await? {
            BeginComplete::Lease(lease) => lease,
            BeginComplete::Held => return Err(OciWriteError::Completing),
            BeginComplete::Unknown => return Err(OciWriteError::NotFound),
        };
        let outcome = self.complete(&done, &lease, now).await;
        if !matches!(outcome, Ok(true)) {
            let _ = self.oci.release_complete(done.upload, &lease).await;
        }
        outcome.map(|_| ())
    }

    /// `Ok(true)` once this completion recorded the blob and removed the
    /// session.
    async fn complete(
        &self,
        done: &Completion<'_>,
        lease: &LeaseToken,
        now: DateTime<Utc>,
    ) -> Result<bool, OciWriteError> {
        let segments = self.oci.segments(done.upload).await?;
        let size: u64 = segments.iter().map(|s| s.len).sum();
        if size == 0 {
            return Err(OciWriteError::Empty);
        }
        let computed = match self.digest(&segments).await {
            Ok(computed) => computed,
            Err(StorageError::NotFound) => return Err(OciWriteError::NotFound),
            Err(e) => return Err(e.into()),
        };
        if computed != done.digest {
            return Err(OciWriteError::DigestMismatch { computed });
        }
        let keys: Vec<String> = segments.iter().map(|s| s.key.clone()).collect();
        let entries = [Entry {
            logical_key: layout::oci_blob_key(done.repo_prefix, hex(done.digest)),
            source: Source::Segments {
                keys: keys.clone(),
                size,
            },
        }];
        let oci = &self.oci;
        let placed = self
            .placer
            .place_shared(
                done.repo_prefix,
                &entries,
                |tokens| async move {
                    let finished = oci
                        .finish_upload(
                            done.upload,
                            lease,
                            &tokens[0],
                            NewBlob {
                                repository: done.repository,
                                digest: done.digest,
                                size: size as i64,
                                content_type: done.content_type,
                            },
                            now,
                        )
                        .await?;
                    match finished {
                        Finished::Recorded(_) => Ok(()),
                        Finished::LeaseLost => Err(StoreError::Conflict),
                    }
                },
                now,
            )
            .await;
        match placed {
            Ok(()) => {
                if let Err(e) = self.storage.delete_batch(&keys).await {
                    warn!(error = %e, upload = %done.upload, "completed upload's segments not deleted; the scan will find them");
                }
                Ok(true)
            }
            Err(PlaceError::Refused(StoreError::Conflict)) => Err(OciWriteError::Completing),
            Err(PlaceError::Retired) => Err(OciWriteError::NotFound),
            Err(PlaceError::Storage(StorageError::NotFound)) => Err(OciWriteError::NotFound),
            Err(e) => Err(e.into()),
        }
    }

    async fn digest(&self, segments: &[Segment]) -> Result<String, StorageError> {
        let mut hasher = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        for segment in segments {
            let mut body = self.storage.read_stream(&segment.key).await?.body;
            loop {
                let n = body.read(&mut buf).await.map_err(|_| StorageError::Unavailable)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        Ok(format!("sha256:{:x}", hasher.finalize()))
    }
}

#[cfg(test)]
#[path = "oci_tests.rs"]
mod tests;
