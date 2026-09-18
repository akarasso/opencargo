use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::db::proxy_cache::CacheEntry;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{CacheRepo, Outcome};
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

impl Outcome<Cached> {
    pub fn into_payload(self) -> Outcome<Payload> {
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
                let (len, reader) = storage.read_stream(path).await.map_err(unreadable)?;
                (len, Body::from_stream(ReaderStream::new(reader)))
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

/// A missing file stays a 404; any other local fault is ours, not the upstream's.
fn unreadable(e: StorageError) -> AppError {
    match e {
        StorageError::NotFound => AppError::NotFound(e.to_string()),
        other => AppError::Internal(format!("stored file unreadable: {other}")),
    }
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

/// One open handle behind an RAII guard: `Drop` unlinks the part unless
/// `commit` renamed it into place, so a refresh never truncates a reader's inode.
pub struct PartFile {
    rel: String,
    resolved: PathBuf,
    file: Option<tokio::fs::File>,
    committed: bool,
}

impl PartFile {
    pub async fn new(storage: &dyn StorageBackend, rel: String) -> AppResult<Self> {
        let resolved = storage.resolve(&rel)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::File::create(&resolved).await?;
        Ok(Self {
            rel,
            resolved,
            file: Some(file),
            committed: false,
        })
    }

    pub async fn write_chunk(&mut self, chunk: &[u8]) -> AppResult<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| AppError::Internal("part file already closed".into()))?;
        file.write_all(chunk).await?;
        Ok(())
    }

    pub async fn commit(mut self, storage: &dyn StorageBackend, final_rel: &str) -> AppResult<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
            file.sync_data().await?;
        }
        storage.rename(&self.rel, final_rel).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    // Synchronous: Drop cannot await and a spawned task would leak at shutdown.
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.resolved);
        }
    }
}
