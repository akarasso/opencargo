//! What a registry knows about an image: its blobs, its manifests, the tags
//! that name them and the uploads still assembling one.
//!
//! The write half is two transactions and two statements. A push lands a
//! manifest, the blobs it references and, for a tag, the mapping — all four
//! or none; a delete takes the same rows away and **returns** what it
//! orphaned rather than deleting a file, because the orphan set does not
//! exist until the rows are gone and a crash after a file delete would leave
//! a live manifest whose layers are not there (§2.4, rule B).

use async_trait::async_trait;

use crate::error::StoreError;

/// A stored blob, as the blob endpoint serves it. The digest and the
/// repository are the caller's, so neither comes back.
pub struct Blob {
    pub size: i64,
    pub content_type: Option<String>,
}

/// A stored manifest, as the manifest endpoint serves it.
pub struct Manifest {
    pub content_type: String,
    pub size: i64,
}

/// A pushed manifest and everything that points at it.
pub struct NewManifest<'a> {
    pub repository: i64,
    pub name: &'a str,
    pub digest: &'a str,
    pub content_type: &'a str,
    pub size: i64,
    /// The blobs this manifest references; the link rows are replaced
    /// wholesale, so a re-push that drops a layer drops its link too.
    pub blobs: &'a [String],
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

/// What a manifest deletion left with nothing pointing at it: the blobs no
/// other manifest of that repository references any more. Their rows are
/// gone; their objects are the caller's to delete, after the commit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Orphaned {
    pub blob_digests: Vec<String>,
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

    /// The manifest digest a reference names. Only ever asked about a tag: a
    /// reference that already is a digest names itself, which the caller
    /// answers without a lookup.
    async fn digest_for_ref(
        &self,
        repository: i64,
        name: &str,
        reference: &str,
    ) -> Result<Option<String>, StoreError>;

    /// Every tag of one image, in tag order — the order the tag listing
    /// serves and paginates.
    async fn tags(&self, repository: i64, name: &str) -> Result<Vec<String>, StoreError>;

    /// The manifest row, its blob links and its tag, written together.
    async fn put_manifest(&self, manifest: NewManifest<'_>) -> Result<(), StoreError>;

    /// Take a manifest, its tags and its blob links away, and report the
    /// blobs that deletion orphaned. `None` when no such manifest was there.
    async fn delete_manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
    ) -> Result<Option<Orphaned>, StoreError>;

    /// Open the ledger entry a chunked push appends to.
    async fn start_upload(&self, id: &str, repository: i64, name: &str) -> Result<(), StoreError>;

    /// The repository an upload was started in; an id is only usable from
    /// there. `None` when the ledger has no such upload.
    async fn upload_owner(&self, id: &str) -> Result<Option<i64>, StoreError>;

    /// Record the assembled blob and close the ledger entry, together: an
    /// upload that survived its own blob would be replayable.
    async fn complete_upload(&self, id: &str, blob: NewBlob<'_>) -> Result<(), StoreError>;

    /// How many manifests of this repository still reference a blob.
    async fn blob_references(&self, repository: i64, digest: &str) -> Result<i64, StoreError>;

    /// Drop a blob row. `false` when there was none.
    async fn delete_blob(&self, repository: i64, digest: &str) -> Result<bool, StoreError>;
}
