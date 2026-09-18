use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;

use crate::domain::{CacheEntry, CacheRepo, Outcome};
use crate::error::{AppError, AppResult};
use crate::storage::{StorageBackend, StorageError};

use super::super::strategy::CacheKey;

#[derive(Debug, Clone)]
pub struct Cached {
    pub entry: CacheEntry,
    pub stale: bool,
}

impl Cached {
    pub fn into_payload(self) -> Payload {
        Payload {
            src: match self.entry.storage_path {
                Some(path) => Src::File(path),
                None => Src::HeadOnly,
            },
            size: self.entry.size.max(0) as u64,
            content_type: self.entry.content_type,
            digest: self.entry.digest,
            stale: self.stale,
        }
    }
}

/// `Outcome` is the domain's word and `Payload` the proxy's, so the map from
/// one to the other is an extension trait here, never an inherent `impl` on a
/// type this layer does not own.
pub trait IntoPayload {
    fn into_payload(self) -> Outcome<Payload>;
}

impl IntoPayload for Outcome<Cached> {
    fn into_payload(self) -> Outcome<Payload> {
        match self {
            Outcome::Found(c) => Outcome::Found(c.into_payload()),
            Outcome::NotFound => Outcome::NotFound,
        }
    }
}

#[derive(Debug)]
pub enum Src {
    File(String),
    Bytes(Bytes),
    HeadOnly,
}

/// The one cache-neutral serving type: hosted rows, cache rows and rewritten
/// JSON all end here.
#[derive(Debug)]
pub struct Payload {
    pub src: Src,
    pub size: u64,
    pub content_type: Option<String>,
    pub digest: Option<String>,
    pub stale: bool,
}

impl Payload {
    pub fn file(storage_path: String, size: u64) -> Self {
        Self {
            src: Src::File(storage_path),
            size,
            content_type: None,
            digest: None,
            stale: false,
        }
    }

    pub fn bytes(b: Bytes) -> Self {
        Self {
            size: b.len() as u64,
            src: Src::Bytes(b),
            content_type: None,
            digest: None,
            stale: false,
        }
    }

    pub fn head_only(size: u64, content_type: Option<String>, digest: Option<String>) -> Self {
        Self {
            src: Src::HeadOnly,
            size,
            content_type,
            digest,
            stale: false,
        }
    }

    /// `Content-Length` describes the bytes actually streamed: the file is
    /// opened once and its length taken from that handle, never from a row
    /// that a refresh may have outrun.
    pub(super) async fn to_response(
        &self,
        storage: &dyn StorageBackend,
        extra: Vec<(HeaderName, HeaderValue)>,
    ) -> AppResult<Response> {
        let (length, body) = match &self.src {
            Src::File(path) => {
                let read = storage.read_stream(path).await.map_err(unreadable)?;
                (read.total, Body::from_stream(ReaderStream::new(read.body)))
            }
            Src::Bytes(b) => (b.len() as u64, Body::from(b.clone())),
            Src::HeadOnly => (self.size, Body::empty()),
        };
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, length);
        if let Some(ct) = &self.content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        if self.stale {
            builder = builder.header(header::WARNING, "110 - \"Response is Stale\"");
        }
        for (name, value) in extra {
            builder = builder.header(name, value);
        }
        builder
            .body(body)
            .map_err(|e| AppError::Internal(format!("response build failed: {e}")))
    }
}

/// A missing file stays a 404; any other fault is ours, never the upstream's,
/// and keeps the storage port's status.
fn unreadable(e: StorageError) -> AppError {
    AppError::from(e)
}

/// `_proxy_cache/{member}/{kind}/{h[..2]}/{h}`, `h = hex(sha256(key))`, so a
/// key that prefixes another never needs one path to be file and directory.
pub fn cache_path(member: CacheRepo<'_>, key: &CacheKey) -> String {
    let h = format!("{:x}", Sha256::digest(key.key.as_bytes()));
    format!(
        "_proxy_cache/{}/{}/{}/{h}",
        member.0.name,
        key.kind,
        &h[..2]
    )
}
