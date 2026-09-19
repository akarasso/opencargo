//! The S3 adapter of the storage port (S3 v5 §2): object_store over any
//! S3-compatible endpoint. The only module that names object_store, a
//! bucket or an endpoint; every fault leaves it as a textless
//! `Unavailable`, its detail logged here.

pub mod cache;
pub mod settings;
mod writer;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{stream, StreamExt, TryStreamExt};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{
    ClientOptions, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig,
};
use tokio::sync::Semaphore;

use crate::ports::clock::Clock;
use crate::ports::multipart::MultipartLedger;
use crate::storage::keys::{self, BACKEND};
use crate::storage::{
    CheckReport, CheckStep, ObjectList, ObjectMeta, ObjectWriter, ReadStream, StorageBackend,
    StorageError, StoreIdentity, UploadPlan,
};

use cache::ExistsCache;
use settings::{tls_roots, S3Settings};

/// The service's key limit, which the prefix counts against.
const S3_MAX_KEY_BYTES: usize = 1024;
/// The largest object one CopyObject can produce, which bounds the blob cap.
const SINGLE_COPY_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const DELETE_BATCH: usize = 1000;
const MIN_CHUNK_BYTES: u64 = 1024 * 1024;
const HEALTH: &str = "health";

pub(crate) fn fault(op: &'static str, err: object_store::Error) -> StorageError {
    if matches!(err, object_store::Error::NotFound { .. }) {
        return StorageError::NotFound;
    }
    tracing::warn!(op, error = %err, "S3 storage fault");
    metrics::counter!("opencargo_storage_faults_total", "backend" => "s3", "op" => op).increment(1);
    StorageError::Unavailable
}

fn timed_out(op: &'static str) -> StorageError {
    tracing::warn!(op, "S3 storage operation exceeded its bound");
    metrics::counter!("opencargo_storage_faults_total", "backend" => "s3", "op" => op).increment(1);
    StorageError::Unavailable
}

pub(crate) struct Inner {
    store: AmazonS3,
    prefix: String,
    identity: StoreIdentity,
    ledger: Arc<dyn MultipartLedger>,
    clock: Arc<dyn Clock>,
    budget: Arc<Semaphore>,
    cache: Option<ExistsCache>,
    part_size: usize,
    completion_timeout: Duration,
    request_timeout: Duration,
}

pub struct S3Storage {
    inner: Arc<Inner>,
    roots: usize,
}

impl Inner {
    fn key_budget(&self) -> usize {
        S3_MAX_KEY_BYTES.saturating_sub(if self.prefix.is_empty() { 0 } else { self.prefix.len() + 1 })
    }

    fn raw_path(&self, rel: &str) -> Result<Path, StorageError> {
        let full = if self.prefix.is_empty() {
            rel.to_string()
        } else if rel.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{rel}", self.prefix)
        };
        Path::parse(full).map_err(|_| StorageError::InvalidPath("invalid storage key".to_string()))
    }

    fn path(&self, key: &str) -> Result<Path, StorageError> {
        keys::validate(key, self.key_budget())?;
        self.raw_path(key)
    }

    /// The private tree of backend-owned objects, which no trait call names.
    fn reserved(&self, rel: &str) -> Path {
        self.raw_path(&format!("{BACKEND}/{rel}"))
            .expect("reserved keys are well formed")
    }

    /// The logical key of a listed location; a location outside the prefix
    /// is a fault, never a key.
    fn logical(&self, location: &Path) -> Result<String, StorageError> {
        let full = location.as_ref();
        if self.prefix.is_empty() {
            return Ok(full.to_string());
        }
        full.strip_prefix(&self.prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_string)
            .ok_or_else(|| {
                tracing::warn!("S3 listing returned a key outside the configured prefix");
                StorageError::Unavailable
            })
    }

    fn is_reserved(key: &str) -> bool {
        let first = key.split('/').next().unwrap_or_default();
        first == keys::SCRATCH || first == BACKEND
    }

    fn meta(&self, key: &str, meta: object_store::ObjectMeta) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size: meta.size,
            last_modified: meta.last_modified,
        }
    }

    async fn stat_path(&self, key: &str, path: &Path) -> Result<Option<ObjectMeta>, StorageError> {
        match tokio::time::timeout(self.request_timeout, self.store.head(path)).await {
            Ok(Ok(meta)) => Ok(Some(self.meta(key, meta))),
            Ok(Err(e)) => match fault("stat", e) {
                StorageError::NotFound => Ok(None),
                other => Err(other),
            },
            Err(_) => Err(timed_out("stat")),
        }
    }

    fn remember(&self, meta: &ObjectMeta) {
        if let Some(cache) = &self.cache {
            cache.fill(meta);
        }
    }

    fn forget(&self, key: &str) {
        if let Some(cache) = &self.cache {
            cache.forget(key);
        }
    }
}

impl S3Storage {
    pub fn build(
        settings: &S3Settings,
        identity: StoreIdentity,
        ledger: Arc<dyn MultipartLedger>,
        clock: Arc<dyn Clock>,
    ) -> anyhow::Result<Self> {
        let roots = tls_roots();
        let mut options = ClientOptions::new()
            .with_allow_http(settings.allow_http)
            .with_no_system_certificates(true)
            .with_connect_timeout(Duration::from_secs(10))
            .with_timeout_disabled();
        for der in &roots {
            options = options.with_root_certificate(object_store::Certificate::from_der(der)?);
        }
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&settings.bucket)
            .with_region(&settings.region)
            .with_access_key_id(&settings.access_key_id)
            .with_secret_access_key(&settings.secret_access_key)
            .with_allow_http(settings.allow_http)
            .with_virtual_hosted_style_request(settings.virtual_hosted_style)
            .with_client_options(options);
        if let Some(endpoint) = &settings.endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        if let Some(token) = &settings.session_token {
            builder = builder.with_token(token);
        }
        if let Some(retries) = settings.max_retries {
            builder = builder.with_retry(RetryConfig {
                max_retries: retries,
                ..RetryConfig::default()
            });
        }
        let store = builder.build()?;
        Ok(Self {
            roots: roots.len(),
            inner: Arc::new(Inner {
                store,
                prefix: settings.prefix.clone(),
                identity,
                ledger,
                clock,
                budget: Arc::new(Semaphore::new(settings.max_multipart_uploads)),
                cache: (settings.exists_cache_entries > 0)
                    .then(|| ExistsCache::new(settings.exists_cache_entries)),
                part_size: settings.part_size,
                completion_timeout: settings.completion_timeout,
                request_timeout: settings.request_timeout,
            }),
        })
    }

    /// How many compiled-in roots the client was fed.
    pub fn trusted_roots(&self) -> usize {
        self.roots
    }

    /// Positive answers `head` may serve without a request.
    pub fn cached_entries(&self) -> usize {
        self.inner.cache.as_ref().map_or(0, ExistsCache::len)
    }

    async fn copy_checked(&self, op: &'static str, from: &str, to: &str) -> Result<(), StorageError> {
        let (from_path, to_path) = (self.inner.path(from)?, self.inner.path(to)?);
        let source = self
            .inner
            .stat_path(from, &from_path)
            .await?
            .ok_or(StorageError::NotFound)?;
        let inner = self.inner.clone();
        let (f, t) = (from_path.clone(), to_path.clone());
        let bound = self.inner.completion_timeout;
        let copied = tokio::spawn(async move {
            tokio::time::timeout(bound, inner.store.copy(&f, &t)).await
        })
        .await
        .map_err(|_| StorageError::Unavailable)?;
        match copied {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let err = fault(op, e);
                if !matches!(err, StorageError::Unavailable) {
                    return Err(err);
                }
            }
            Err(_) => {
                timed_out(op);
            }
        }
        match self.inner.stat_path(to, &to_path).await? {
            Some(meta) if meta.size == source.size => {
                self.inner.remember(&meta);
                Ok(())
            }
            _ => Err(StorageError::Unavailable),
        }
    }

    fn list_prefix(&self, prefix: &str) -> ObjectList {
        let inner = self.inner.clone();
        let exact = if prefix.is_empty() {
            None
        } else {
            Some(prefix.to_string())
        };
        let root = match inner.raw_path(prefix) {
            Ok(root) => root,
            Err(e) => return Box::pin(stream::once(async move { Err(e) })),
        };
        let listing_root = if prefix.is_empty() && inner.prefix.is_empty() {
            None
        } else {
            Some(root)
        };
        let head = {
            let inner = inner.clone();
            stream::once(async move {
                match exact {
                    Some(key) => match inner.raw_path(&key) {
                        Ok(path) => inner.stat_path(&key, &path).await,
                        Err(e) => Err(e),
                    },
                    None => Ok(None),
                }
            })
            .filter_map(|r| async move { r.transpose() })
        };
        let idle = inner.request_timeout;
        let children = idle_bounded(inner.store.list(listing_root.as_ref()), idle)
            .map(move |entry| {
                let meta = entry.map_err(|e| fault("list", e))?;
                let key = inner.logical(&meta.location)?;
                Ok(Some(inner.meta(&key, meta)))
            })
            .try_filter_map(|m: Option<ObjectMeta>| async move {
                Ok(m.filter(|m| !Inner::is_reserved(&m.key)))
            });
        Box::pin(head.chain(children))
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        let path = self.inner.path(key)?;
        let bound = self.inner.completion_timeout;
        let got = tokio::time::timeout(bound, async {
            let got = self.inner.store.get(&path).await?;
            got.bytes().await
        })
        .await
        .map_err(|_| timed_out("get"))?;
        got.map_err(|e| fault("get", e))
    }

    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        let path = self.inner.path(key)?;
        Ok(Box::new(writer::S3Writer::new(self.inner.clone(), key.to_string(), path)))
    }

    async fn read_stream(&self, key: &str) -> Result<ReadStream, StorageError> {
        let path = self.inner.path(key)?;
        let got = tokio::time::timeout(self.inner.request_timeout, self.inner.store.get(&path))
            .await
            .map_err(|_| timed_out("read"))?
            .map_err(|e| fault("read", e))?;
        let total = got.meta.size;
        let body = idle_bounded(got.into_stream(), self.inner.request_timeout)
            .map_err(|e| std::io::Error::other(fault("read", e)));
        Ok(ReadStream {
            total,
            body: Box::pin(tokio_util::io::StreamReader::new(body)),
        })
    }

    async fn copy_object(&self, from: &str, to: &str) -> Result<(), StorageError> {
        self.copy_checked("copy", from, to).await
    }

    async fn relocate(&self, from: &str, to: &str) -> Result<(), StorageError> {
        match self.copy_checked("relocate", from, to).await {
            Ok(()) => {}
            Err(StorageError::NotFound) => {
                let path = self.inner.path(to)?;
                return match self.inner.stat_path(to, &path).await? {
                    Some(_) => Ok(()),
                    None => Err(StorageError::NotFound),
                };
            }
            Err(e) => return Err(e),
        }
        let from_path = self.inner.path(from)?;
        if let Err(e) = self.inner.store.delete(&from_path).await {
            tracing::warn!(error = %e, "relocate left its source behind");
        }
        self.inner.forget(from);
        Ok(())
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let path = self.inner.path(key)?;
        if let Some(meta) = self.inner.cache.as_ref().and_then(|c| c.get(key)) {
            return Ok(Some(meta));
        }
        let found = self.inner.stat_path(key, &path).await?;
        if let Some(meta) = &found {
            self.inner.remember(meta);
        }
        Ok(found)
    }

    async fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let path = self.inner.path(key)?;
        self.inner.stat_path(key, &path).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = self.inner.path(key)?;
        self.inner.forget(key);
        match tokio::time::timeout(self.inner.request_timeout, self.inner.store.delete(&path)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => match fault("delete", e) {
                StorageError::NotFound => Ok(()),
                other => Err(other),
            },
            Err(_) => Err(timed_out("delete")),
        }
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), StorageError> {
        let paths = keys
            .iter()
            .map(|k| self.inner.path(k))
            .collect::<Result<Vec<_>, _>>()?;
        for key in keys {
            self.inner.forget(key);
        }
        for chunk in paths.chunks(DELETE_BATCH) {
            let owned: Vec<Path> = chunk.to_vec();
            let locations = stream::iter(owned.into_iter().map(Ok)).boxed();
            let results: Vec<_> = tokio::time::timeout(
                self.inner.request_timeout * 2,
                self.inner.store.delete_stream(locations).collect::<Vec<_>>(),
            )
            .await
            .map_err(|_| timed_out("delete_batch"))?;
            for result in results {
                if let Err(e) = result {
                    match fault("delete_batch", e) {
                        StorageError::NotFound => {}
                        other => return Err(other),
                    }
                }
            }
        }
        Ok(())
    }

    fn list(&self, prefix: &str) -> ObjectList {
        if let Err(e) = keys::validate_prefix(prefix, self.inner.key_budget()) {
            return Box::pin(stream::once(async move { Err(e) }));
        }
        self.list_prefix(prefix)
    }

    async fn sweep_abandoned(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StorageError> {
        writer::sweep(&self.inner, older_than, now).await
    }

    async fn probe(&self) -> Result<(), StorageError> {
        let path = self.inner.reserved(HEALTH);
        match self.inner.stat_path(HEALTH, &path).await? {
            Some(_) => Ok(()),
            None => self
                .inner
                .store
                .put(&path, PutPayload::from_static(b"ok"))
                .await
                .map(|_| ())
                .map_err(|e| fault("probe", e)),
        }
    }

    fn key_budget(&self) -> keys::KeyBudget {
        // An S3 key is opaque to the protocol, but the server behind it may
        // store each object as a file -- MinIO does -- and then every segment
        // is a file name its own filesystem bounds. We cannot ask the remote,
        // so the segment is the one a filesystem allows: exceeding it answers
        // a fault, not a refusal the client can read.
        keys::KeyBudget { key: self.inner.key_budget(), segment: keys::MAX_NAME_BYTES }
    }

    fn upload_plan(&self) -> UploadPlan {
        UploadPlan {
            max_object_bytes: SINGLE_COPY_BYTES,
            min_chunk_bytes: MIN_CHUNK_BYTES,
            completion_bound: self.inner.completion_timeout,
            delete_bound: self.inner.request_timeout * 2,
            delete_batch: DELETE_BATCH,
        }
    }

    async fn self_check(&self) -> CheckReport {
        self_check(&self.inner).await
    }

    fn identity(&self) -> StoreIdentity {
        self.inner.identity.clone()
    }
}

/// A body stream that fails once the wire stays silent for `idle` while its
/// reader waits on it; a reader that is slow to ask is never counted.
fn idle_bounded<T: Send + 'static>(
    inner: futures_util::stream::BoxStream<'static, object_store::Result<T>>,
    idle: Duration,
) -> futures_util::stream::BoxStream<'static, object_store::Result<T>> {
    stream::unfold(Some(inner), move |state| async move {
        let mut inner = state?;
        match tokio::time::timeout(idle, inner.next()).await {
            Ok(Some(item)) => Some((item, Some(inner))),
            Ok(None) => None,
            Err(_) => Some((
                Err(object_store::Error::Generic {
                    store: "S3",
                    source: "the object stream stayed idle past the request timeout".into(),
                }),
                None,
            )),
        }
    })
    .boxed()
}

/// Every operation on a private tree, cleaned up after itself.
async fn self_check(inner: &Arc<Inner>) -> CheckReport {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let base = format!("self-check/{nonce}");
    let (a, b, big) = (
        inner.reserved(&format!("{base}/a")),
        inner.reserved(&format!("{base}/b")),
        inner.reserved(&format!("{base}/multipart")),
    );
    let mut steps = Vec::new();
    let mut record = |operation: &'static str, outcome: Result<(), String>| {
        steps.push(CheckStep { operation, outcome });
    };
    let text = |e: object_store::Error| e.to_string();

    record("put", inner.store.put(&a, PutPayload::from_static(b"self-check")).await.map(|_| ()).map_err(text));
    record(
        "get",
        match inner.store.get(&a).await {
            Ok(got) => match got.bytes().await {
                Ok(bytes) if bytes.as_ref() == b"self-check" => Ok(()),
                Ok(_) => Err("read back different bytes".to_string()),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(e.to_string()),
        },
    );
    record("copy", inner.store.copy(&a, &b).await.map_err(text));
    record(
        "head",
        match inner.store.head(&b).await {
            Ok(meta) if meta.size == 10 => Ok(()),
            Ok(meta) => Err(format!("copied {} bytes of 10", meta.size)),
            Err(e) => Err(e.to_string()),
        },
    );
    let listing = inner.reserved(&base);
    record(
        "list",
        match inner.store.list(Some(&listing)).try_collect::<Vec<_>>().await {
            Ok(found) if found.len() == 2 => Ok(()),
            Ok(found) => Err(format!("listed {} objects of 2", found.len())),
            Err(e) => Err(e.to_string()),
        },
    );
    let multipart = writer::check_multipart(inner, &big, "self-check-multipart").await;
    record("multipart", multipart);
    let doomed = stream::iter([a, b, big].map(Ok)).boxed();
    let deleted: Vec<_> = inner.store.delete_stream(doomed).collect().await;
    record(
        "delete",
        deleted
            .into_iter()
            .find_map(Result::err)
            .map_or(Ok(()), |e| Err(e.to_string())),
    );
    CheckReport { steps }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod relay_tests;
