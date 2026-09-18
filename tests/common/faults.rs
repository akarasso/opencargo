//! Ports that go down on demand, for proving what a request answers when
//! the database or the object store is unavailable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};

use opencargo::domain::Rights;
use opencargo::error::StoreError;
use opencargo::ports::permissions::{PermissionStore, RepoRights};
use opencargo::proxy::{ProxyEngine, Timeouts, TtlConfig};
use opencargo::server::AppState;
use opencargo::storage::{
    CheckReport, ObjectList, ObjectMeta, ObjectWriter, ReadStream, StorageBackend, StorageError,
    StoreIdentity, UploadPlan,
};

/// One switch per port: while it is on, every call answers `Unavailable`.
#[derive(Clone, Default)]
pub struct Outage {
    pub storage: Arc<AtomicBool>,
    pub permissions: Arc<AtomicBool>,
}

impl Outage {
    /// Puts both ports of `state` behind this outage's switches.
    pub fn install(&self, state: &mut AppState) {
        let storage: Arc<dyn StorageBackend> = Arc::new(DownStorage {
            inner: state.storage.clone(),
            down: self.storage.clone(),
        });
        state.permissions = Arc::new(DownPermissions {
            inner: state.permissions.clone(),
            down: self.permissions.clone(),
        });
        state.proxy = ProxyEngine::new(
            storage.clone(),
            state.cache.clone(),
            state.repos.clone(),
            state.reclaim.clone(),
            Timeouts::from_connect_secs(10),
            TtlConfig {
                default_secs: 24 * 3600,
                negative_secs: 3600,
            },
        );
        state.storage = storage;
    }

    pub fn set(&self, storage: bool, permissions: bool) {
        self.storage.store(storage, Ordering::SeqCst);
        self.permissions.store(permissions, Ordering::SeqCst);
    }
}

struct DownStorage {
    inner: Arc<dyn StorageBackend>,
    down: Arc<AtomicBool>,
}

impl DownStorage {
    fn up(&self) -> Result<(), StorageError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StorageError::Unavailable)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl StorageBackend for DownStorage {
    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        self.up()?;
        self.inner.get(key).await
    }
    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        self.up()?;
        self.inner.writer(key).await
    }
    async fn read_stream(&self, key: &str) -> Result<ReadStream, StorageError> {
        self.up()?;
        self.inner.read_stream(key).await
    }
    async fn copy_object(&self, from: &str, to: &str) -> Result<(), StorageError> {
        self.up()?;
        self.inner.copy_object(from, to).await
    }
    async fn relocate(&self, from: &str, to: &str) -> Result<(), StorageError> {
        self.up()?;
        self.inner.relocate(from, to).await
    }
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        self.up()?;
        self.inner.head(key).await
    }
    async fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        self.up()?;
        self.inner.stat(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.up()?;
        self.inner.delete(key).await
    }
    async fn delete_batch(&self, keys: &[String]) -> Result<(), StorageError> {
        self.up()?;
        self.inner.delete_batch(keys).await
    }
    fn list(&self, prefix: &str) -> ObjectList {
        self.inner.list(prefix)
    }
    async fn sweep_abandoned(&self, older_than: Duration, now: DateTime<Utc>) -> Result<u64, StorageError> {
        self.up()?;
        self.inner.sweep_abandoned(older_than, now).await
    }
    async fn probe(&self) -> Result<(), StorageError> {
        self.up()?;
        self.inner.probe().await
    }
    fn upload_plan(&self) -> UploadPlan {
        self.inner.upload_plan()
    }
    async fn self_check(&self) -> CheckReport {
        self.inner.self_check().await
    }
    fn identity(&self) -> StoreIdentity {
        self.inner.identity()
    }
}

struct DownPermissions {
    inner: Arc<dyn PermissionStore>,
    down: Arc<AtomicBool>,
}

impl DownPermissions {
    fn up(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StoreError::Unavailable)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl PermissionStore for DownPermissions {
    async fn rights(&self, user_id: i64, repository_id: i64) -> Result<Option<Rights>, StoreError> {
        self.up()?;
        self.inner.rights(user_id, repository_id).await
    }
    async fn of_user(&self, user_id: i64) -> Result<Vec<RepoRights>, StoreError> {
        self.up()?;
        self.inner.of_user(user_id).await
    }
    async fn set(&self, user_id: i64, repository_id: i64, rights: Rights, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.up()?;
        self.inner.set(user_id, repository_id, rights, now).await
    }
    async fn revoke(&self, user_id: i64, repository_id: i64) -> Result<(), StoreError> {
        self.up()?;
        self.inner.revoke(user_id, repository_id).await
    }
}
