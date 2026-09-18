//! The streamed writer: a body that stays under one part is one PUT; a
//! larger one becomes a multipart upload, opened only once its bytes would
//! exceed a part, under the process-wide multipart budget and recorded in
//! the ledger so a crashed writer's upload is swept. Completion runs on a
//! task of its own, so a dropped caller cannot interrupt it, and a
//! completion that fails or overruns its bound is decided by `stat`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use object_store::multipart::{MultipartStore, PartId};
use object_store::path::Path;
use object_store::{ObjectStoreExt, PutPayload};
use tokio::sync::OwnedSemaphorePermit;

use super::{fault, timed_out, Inner};
use crate::storage::{ObjectMeta, ObjectWriter, StorageError};

/// A ledger bump at most this often, measured on the monotonic clock.
const TOUCH_EVERY: Duration = Duration::from_secs(15 * 60);

/// The ledger's rows are shared by every store of the process: each one
/// sees only its own.
fn scope(identity: &str) -> String {
    format!("{identity}\u{1f}")
}

struct Open {
    id: String,
    parts: Vec<PartId>,
    touched: Instant,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct S3Writer {
    inner: Arc<Inner>,
    key: String,
    path: Path,
    buf: BytesMut,
    open: Option<Open>,
    written: u64,
}

impl S3Writer {
    pub(crate) fn new(inner: Arc<Inner>, key: String, path: Path) -> Self {
        Self {
            inner,
            key,
            path,
            buf: BytesMut::new(),
            open: None,
            written: 0,
        }
    }

    fn ledger_key(&self) -> String {
        format!("{}{}", scope(&self.inner.identity.0), self.path.as_ref())
    }

    async fn open_upload(&mut self) -> Result<(), StorageError> {
        let permit = self
            .inner
            .budget
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StorageError::Unavailable)?;
        let id = tokio::time::timeout(
            self.inner.request_timeout,
            self.inner.store.create_multipart(&self.path),
        )
        .await
        .map_err(|_| timed_out("multipart"))?
        .map_err(|e| fault("multipart", e))?;
        let now = self.inner.clock.now();
        if let Err(e) = self.inner.ledger.opened(&id, &self.ledger_key(), now).await {
            tracing::warn!(error = %e, "multipart ledger refused an upload; aborting it");
            let _ = self.inner.store.abort_multipart(&self.path, &id).await;
            return Err(StorageError::Unavailable);
        }
        self.open = Some(Open {
            id,
            parts: Vec::new(),
            touched: Instant::now(),
            _permit: permit,
        });
        Ok(())
    }

    async fn put_part(&mut self, data: Bytes) -> Result<(), StorageError> {
        let inner = self.inner.clone();
        let open = self.open.as_mut().expect("a part needs an open upload");
        let part = tokio::time::timeout(
            inner.completion_timeout,
            inner
                .store
                .put_part(&self.path, &open.id, open.parts.len(), PutPayload::from(data)),
        )
        .await
        .map_err(|_| timed_out("part"))?
        .map_err(|e| fault("part", e))?;
        open.parts.push(part);
        if open.touched.elapsed() >= TOUCH_EVERY {
            open.touched = Instant::now();
            if let Err(e) = inner.ledger.touched(&open.id, inner.clock.now()).await {
                tracing::warn!(error = %e, "multipart ledger touch failed");
            }
        }
        Ok(())
    }

    async fn drain_full_parts(&mut self) -> Result<(), StorageError> {
        while self.open.is_some() && self.buf.len() >= self.inner.part_size {
            let part = self.buf.split_to(self.inner.part_size).freeze();
            self.put_part(part).await?;
        }
        Ok(())
    }

    fn remember(&self, size: u64) {
        self.inner.remember(&ObjectMeta {
            key: self.key.clone(),
            size,
            last_modified: Utc::now(),
        });
    }
}

#[async_trait]
impl ObjectWriter for S3Writer {
    async fn reserve(&mut self, next_len: usize) -> Result<(), StorageError> {
        if self.open.is_none() && self.buf.len() + next_len > self.inner.part_size {
            self.open_upload().await?;
        }
        Ok(())
    }

    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError> {
        self.written += chunk.len() as u64;
        self.buf.extend_from_slice(&chunk);
        if self.open.is_none() && self.buf.len() > self.inner.part_size {
            self.open_upload().await?;
        }
        self.drain_full_parts().await
    }

    async fn commit(mut self: Box<Self>) -> Result<u64, StorageError> {
        let written = self.written;
        let rest = std::mem::take(&mut self.buf).freeze();
        if self.open.is_none() {
            let inner = self.inner.clone();
            let path = self.path.clone();
            let key = self.key.clone();
            let put = tokio::spawn(async move {
                let landed = tokio::time::timeout(
                    inner.completion_timeout,
                    inner.store.put(&path, PutPayload::from(rest)),
                )
                .await;
                decide(&inner, "put", &key, &path, written, landed.map(|r| r.map(|_| ()))).await
            });
            put.await.map_err(|_| StorageError::Unavailable)??;
            self.remember(written);
            return Ok(written);
        }
        if !rest.is_empty() || self.open.as_ref().is_some_and(|o| o.parts.is_empty()) {
            self.put_part(rest).await?;
        }
        let open = self.open.take().expect("checked above");
        let inner = self.inner.clone();
        let path = self.path.clone();
        let key = self.key.clone();
        let completion = tokio::spawn(async move {
            let landed = tokio::time::timeout(
                inner.completion_timeout,
                inner.store.complete_multipart(&path, &open.id, open.parts),
            )
            .await;
            let decided =
                decide(&inner, "complete", &key, &path, written, landed.map(|r| r.map(|_| ()))).await;
            if decided.is_ok() {
                if let Err(e) = inner.ledger.closed(&open.id).await {
                    tracing::warn!(error = %e, "multipart ledger row left for the sweep");
                }
            }
            drop(open._permit);
            decided
        });
        completion.await.map_err(|_| StorageError::Unavailable)??;
        self.remember(written);
        Ok(written)
    }
}

/// A write that answered anything but success is re-decided by an uncached
/// `stat` of its target: the expected size means it landed.
async fn decide(
    inner: &Inner,
    op: &'static str,
    key: &str,
    path: &Path,
    written: u64,
    landed: Result<Result<(), object_store::Error>, tokio::time::error::Elapsed>,
) -> Result<(), StorageError> {
    match landed {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(e)) => {
            fault(op, e);
        }
        Err(_) => {
            timed_out(op);
        }
    }
    match inner.stat_path(key, path).await? {
        Some(meta) if meta.size == written => Ok(()),
        _ => Err(StorageError::Unavailable),
    }
}

impl Drop for S3Writer {
    fn drop(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = self.inner.clone();
        let path = self.path.clone();
        runtime.spawn(async move {
            match inner.store.abort_multipart(&path, &open.id).await {
                Ok(()) => {
                    let _ = inner.ledger.closed(&open.id).await;
                }
                Err(e) => tracing::warn!(error = %e, "dropped writer's upload left for the sweep"),
            }
        });
    }
}

/// Aborts every upload of this store the ledger shows idle for `older_than`.
pub(crate) async fn sweep(
    inner: &Inner,
    older_than: Duration,
    now: DateTime<Utc>,
) -> Result<u64, StorageError> {
    let idle = inner
        .ledger
        .idle_since(older_than, now)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "multipart ledger unreadable");
            StorageError::Unavailable
        })?;
    let scope = scope(&inner.identity.0);
    let mut swept = 0;
    for (id, key) in idle {
        let Some(location) = key.strip_prefix(&scope) else {
            continue;
        };
        let Ok(path) = Path::parse(location) else {
            continue;
        };
        match inner.store.abort_multipart(&path, &id).await {
            Ok(()) => {}
            Err(e) => match fault("sweep", e) {
                StorageError::NotFound => {}
                _ => continue,
            },
        }
        if inner.ledger.closed(&id).await.is_ok() {
            swept += 1;
        }
    }
    Ok(swept)
}

/// One multipart upload of one small part, completed and checked by size.
pub(crate) async fn check_multipart(inner: &Inner, path: &Path, body: &'static str) -> Result<(), String> {
    let id = inner.store.create_multipart(path).await.map_err(|e| e.to_string())?;
    let part = inner
        .store
        .put_part(path, &id, 0, PutPayload::from_static(body.as_bytes()))
        .await
        .map_err(|e| e.to_string())?;
    inner
        .store
        .complete_multipart(path, &id, vec![part])
        .await
        .map_err(|e| e.to_string())?;
    match inner.store.head(path).await {
        Ok(meta) if meta.size == body.len() as u64 => Ok(()),
        Ok(meta) => Err(format!("completed {} bytes of {}", meta.size, body.len())),
        Err(e) => Err(e.to_string()),
    }
}
