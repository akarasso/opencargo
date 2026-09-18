//! An in-memory `StorageBackend` for use-case tests: it records every delete,
//! can fail an operation on demand, and passes `storage_contract!`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use futures_util::stream;

use crate::storage::keys::{self, MAX_KEY_BYTES};
use crate::storage::{
    CheckReport, CheckStep, ObjectList, ObjectMeta, ObjectWriter, ReadStream, StorageBackend,
    StorageError, StoreIdentity, UploadPlan, Versioning,
};

type Objects = Arc<Mutex<BTreeMap<String, (Bytes, DateTime<Utc>)>>>;
type Noncurrent = Arc<Mutex<BTreeMap<String, Vec<(Bytes, DateTime<Utc>)>>>>;

#[derive(Default, Clone)]
pub struct MemStorage {
    objects: Objects,
    deleted: Arc<Mutex<Vec<String>>>,
    fail: Arc<Mutex<Vec<&'static str>>>,
    /// What an overwrite or a delete left behind, newest last, when the
    /// store keeps noncurrent versions.
    noncurrent: Noncurrent,
    keeps_versions: Arc<Mutex<bool>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that keeps what an overwrite or a delete replaced, as a
    /// versioned bucket does.
    pub fn versioned(self) -> Self {
        *self.keeps_versions.lock().unwrap() = true;
        self
    }

    fn keep(&self, key: &str) {
        if !*self.keeps_versions.lock().unwrap() {
            return;
        }
        let Some(previous) = self.objects.lock().unwrap().get(key).cloned() else {
            return;
        };
        self.noncurrent
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .push(previous);
    }

    /// The next call of `op` answers `Unavailable`.
    pub fn fail_next(&self, op: &'static str) {
        self.fail.lock().unwrap().push(op);
    }

    /// Every key a `delete`/`delete_batch` removed, in order.
    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.objects.lock().unwrap().contains_key(key)
    }

    /// Every key but the store's own tree, which no caller of this port
    /// ever names.
    pub fn keys(&self) -> Vec<String> {
        self.objects
            .lock()
            .unwrap()
            .keys()
            .filter(|k| !crate::domain::layout::reserved(k))
            .cloned()
            .collect()
    }

    /// Removes an object behind the port's back, as a racing reclaimer would.
    pub fn vanish(&self, key: &str) {
        self.objects.lock().unwrap().remove(key);
    }

    fn check(&self, op: &'static str) -> Result<(), StorageError> {
        let mut fail = self.fail.lock().unwrap();
        match fail.iter().position(|f| *f == op) {
            Some(i) => {
                fail.remove(i);
                Err(StorageError::Unavailable)
            }
            None => Ok(()),
        }
    }

    fn meta(&self, key: &str) -> Option<ObjectMeta> {
        self.objects.lock().unwrap().get(key).map(|(b, at)| ObjectMeta {
            key: key.to_string(),
            size: b.len() as u64,
            last_modified: *at,
        })
    }
}

struct MemWriter {
    key: String,
    buf: BytesMut,
    objects: Objects,
    store: MemStorage,
}

#[async_trait]
impl ObjectWriter for MemWriter {
    async fn reserve(&mut self, _next_len: usize) -> Result<(), StorageError> {
        Ok(())
    }

    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError> {
        self.buf.extend_from_slice(&chunk);
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<u64, StorageError> {
        let size = self.buf.len() as u64;
        self.store.keep(&self.key);
        self.objects
            .lock()
            .unwrap()
            .insert(self.key, (self.buf.freeze(), Utc::now()));
        Ok(size)
    }
}

#[async_trait]
impl StorageBackend for MemStorage {
    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        self.check("get")?;
        let objects = self.objects.lock().unwrap();
        objects.get(key).map(|(b, _)| b.clone()).ok_or(StorageError::NotFound)
    }

    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        self.check("writer")?;
        Ok(Box::new(MemWriter {
            key: key.to_string(),
            buf: BytesMut::new(),
            objects: self.objects.clone(),
            store: self.clone(),
        }))
    }

    async fn read_stream(&self, key: &str) -> Result<ReadStream, StorageError> {
        let body = self.get(key).await?;
        Ok(ReadStream {
            total: body.len() as u64,
            body: Box::pin(std::io::Cursor::new(body)),
        })
    }

    async fn copy_object(&self, from: &str, to: &str) -> Result<(), StorageError> {
        keys::validate(from, MAX_KEY_BYTES)?;
        keys::validate(to, MAX_KEY_BYTES)?;
        self.check("copy_object")?;
        let mut objects = self.objects.lock().unwrap();
        let body = objects.get(from).map(|(b, _)| b.clone()).ok_or(StorageError::NotFound)?;
        objects.insert(to.to_string(), (body, Utc::now()));
        Ok(())
    }

    async fn relocate(&self, from: &str, to: &str) -> Result<(), StorageError> {
        keys::validate(from, MAX_KEY_BYTES)?;
        keys::validate(to, MAX_KEY_BYTES)?;
        self.check("relocate")?;
        let mut objects = self.objects.lock().unwrap();
        match objects.remove(from) {
            Some((body, _)) => {
                objects.insert(to.to_string(), (body, Utc::now()));
                Ok(())
            }
            None if objects.contains_key(to) => Ok(()),
            None => Err(StorageError::NotFound),
        }
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        self.stat(key).await
    }

    async fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        self.check("stat")?;
        Ok(self.meta(key))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        self.check("delete")?;
        self.keep(key);
        self.objects.lock().unwrap().remove(key);
        self.deleted.lock().unwrap().push(key.to_string());
        Ok(())
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), StorageError> {
        for key in keys {
            keys::validate(key, MAX_KEY_BYTES)?;
        }
        self.check("delete_batch")?;
        for key in keys {
            self.keep(key);
        }
        let mut objects = self.objects.lock().unwrap();
        let mut deleted = self.deleted.lock().unwrap();
        for key in keys {
            objects.remove(key);
            deleted.push(key.clone());
        }
        Ok(())
    }

    fn list(&self, prefix: &str) -> ObjectList {
        if let Err(e) = keys::validate_prefix(prefix, MAX_KEY_BYTES) {
            return Box::pin(stream::once(async move { Err(e) }));
        }
        if let Err(e) = self.check("list") {
            return Box::pin(stream::once(async move { Err(e) }));
        }
        let found: Vec<Result<ObjectMeta, StorageError>> = self
            .objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| keys::under(k, prefix))
            .map(|(k, (b, at))| {
                Ok(ObjectMeta {
                    key: k.clone(),
                    size: b.len() as u64,
                    last_modified: *at,
                })
            })
            .collect();
        Box::pin(stream::iter(found))
    }

    async fn sweep_abandoned(
        &self,
        _older_than: Duration,
        _now: DateTime<Utc>,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn probe(&self) -> Result<(), StorageError> {
        self.check("probe")
    }

    async fn versioning(&self) -> Result<Versioning, StorageError> {
        Ok(if *self.keeps_versions.lock().unwrap() {
            Versioning::Kept
        } else {
            Versioning::NotKept
        })
    }

    async fn restore_last_version(&self, key: &str) -> Result<bool, StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        self.check("restore_last_version")?;
        let previous = self.noncurrent.lock().unwrap().get_mut(key).and_then(Vec::pop);
        match previous {
            Some(version) => {
                self.objects.lock().unwrap().insert(key.to_string(), version);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn upload_plan(&self) -> UploadPlan {
        UploadPlan {
            max_object_bytes: u64::MAX,
            min_chunk_bytes: 0,
            completion_bound: Duration::from_secs(1),
            delete_bound: Duration::from_secs(1),
            delete_batch: 2,
        }
    }

    async fn self_check(&self) -> CheckReport {
        CheckReport {
            steps: vec![CheckStep {
                operation: "memory",
                outcome: Ok(()),
            }],
        }
    }

    fn identity(&self) -> StoreIdentity {
        StoreIdentity("memory".to_string())
    }
}

mod contract {
    use super::*;

    crate::storage_contract!(async {
        let s: Arc<dyn StorageBackend> = Arc::new(MemStorage::new());
        ((), s)
    });
}
