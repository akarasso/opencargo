//! What a registry knows about an image: its blobs, its manifests, the tags
//! that name them and the upload sessions still assembling one.
//!
//! Rows record the physical key their bytes live at and never re-derive it.
//! A commit that makes a row reference a shared key spends the placement's
//! pins by compare-and-set in its own transaction and answers `Superseded`,
//! writing nothing, when one was revoked (A1 C5bis, M1). A delete enqueues
//! what it released in its own transaction and deletes nothing (M2).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;
use crate::ports::reclaim::PinToken;

/// A stored blob, as the blob endpoint serves it.
pub struct Blob {
    pub size: i64,
    pub content_type: Option<String>,
    pub key: String,
}

/// A stored manifest, as the manifest endpoint serves it.
pub struct Manifest {
    pub content_type: String,
    pub size: i64,
    pub key: String,
}

/// A pushed manifest and everything that points at it.
pub struct NewManifest<'a> {
    pub repository: i64,
    pub name: &'a str,
    pub digest: &'a str,
    pub content_type: &'a str,
    pub size: i64,
    /// The pin of the manifest's own bytes.
    pub pin: &'a PinToken,
    /// The config and layers, each with the pin taken on its current
    /// generation; every one must have a committed blob row.
    pub blobs: &'a [(String, PinToken)],
    /// The child manifests of an index, linked but not pinned.
    pub children: &'a [String],
    /// The tag the push named, absent when the reference was already a digest.
    pub tag: Option<&'a str>,
}

/// A blob an upload just completed.
pub struct NewBlob<'a> {
    pub repository: i64,
    pub digest: &'a str,
    pub size: i64,
    pub content_type: &'a str,
}

/// What a manifest deletion left with nothing pointing at it. Their rows are
/// gone and their keys enqueued in the same transaction.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Orphaned {
    pub blob_digests: Vec<String>,
}

/// An upload session as a chunk or a completion sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSession {
    pub repository: i64,
    /// Every segment of the session lies under it.
    pub prefix: String,
    pub received: u64,
    pub segments: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub len: u64,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentClaim {
    Won,
    /// Another chunk took this offset, or the session is gone.
    Lost,
    TooManySegments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginComplete {
    Lease(LeaseToken),
    /// A live lease is held by another completion.
    Held,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// The row references this key: the pinned one, or the one a row
    /// already recorded, in which case the pinned generation was enqueued.
    Recorded(String),
    /// The lease was taken over: nothing was written.
    LeaseLost,
}

#[async_trait]
pub trait OciStore: Send + Sync {
    async fn blob(&self, repository: i64, digest: &str) -> Result<Option<Blob>, StoreError>;

    async fn manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
    ) -> Result<Option<Manifest>, StoreError>;

    /// The manifest digest a tag names.
    async fn digest_for_ref(
        &self,
        repository: i64,
        name: &str,
        reference: &str,
    ) -> Result<Option<String>, StoreError>;

    /// Every tag of one image, in tag order.
    async fn tags(&self, repository: i64, name: &str) -> Result<Vec<String>, StoreError>;

    /// The manifest row, its links and its tag, written together once every
    /// pin holds and every blob has a row. `NotFound` names no blob: the
    /// caller answers the manifest's blobs unknown.
    async fn put_manifest(&self, manifest: NewManifest<'_>, now: DateTime<Utc>)
        -> Result<(), StoreError>;

    /// Take a manifest, its tags and its links away, enqueue its key and the
    /// keys of the blobs it orphaned. `None` when no such manifest was there.
    async fn delete_manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Orphaned>, StoreError>;

    /// How many manifests of this repository still reference a blob.
    async fn blob_references(&self, repository: i64, digest: &str) -> Result<i64, StoreError>;

    /// Drop an unreferenced blob row and enqueue its key. `false` when there
    /// was none; `Conflict` when a manifest still lists it.
    async fn delete_blob(
        &self,
        repository: i64,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;

    async fn start_upload(
        &self,
        id: &str,
        repository: i64,
        name: &str,
        prefix: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// `None` for an unknown id, and for a session started before upload
    /// progress was recorded.
    async fn upload(&self, id: &str) -> Result<Option<UploadSession>, StoreError>;

    /// The segment row and the progress, by compare-and-set on `start`.
    async fn claim_segment(
        &self,
        id: &str,
        segment: &Segment,
        max_segments: u32,
        now: DateTime<Utc>,
    ) -> Result<SegmentClaim, StoreError>;

    /// In offset order.
    async fn segments(&self, id: &str) -> Result<Vec<Segment>, StoreError>;

    /// A dated lease with a fresh token; an expired one is taken over.
    async fn begin_complete(
        &self,
        id: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<BeginComplete, StoreError>;

    /// A stale token is a no-op.
    async fn release_complete(&self, id: &str, lease: &LeaseToken) -> Result<(), StoreError>;

    /// The blob row with the pinned key, the session and its segment rows
    /// removed, the pin spent: all conditional on the lease and the pin.
    async fn finish_upload(
        &self,
        id: &str,
        lease: &LeaseToken,
        pin: &PinToken,
        blob: NewBlob<'_>,
        now: DateTime<Utc>,
    ) -> Result<Finished, StoreError>;

    /// Sessions idle for `idle` with no live lease, and every session started
    /// before upload progress was recorded: rows removed, prefixes enqueued.
    async fn reap_uploads(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError>;
}
