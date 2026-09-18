//! What a registry knows about an image: its blobs, its manifests and the
//! tags that name them.
//!
//! Read-only for now. The push half — a manifest and its blob links written
//! together, a delete and the objects it orphans — is one transaction and
//! arrives with the commit that moves the OCI writes off the pool; a leaf
//! only ever reads, and the resolver cannot lose its pool before it does.

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
}
