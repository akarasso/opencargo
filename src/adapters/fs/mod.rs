// The filesystem adapter of the storage port. The allows are per item, not per
// file, so that the sqlx entries of clippy.toml stay live here.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::storage::keys::{self, BACKEND, MAX_KEY_BYTES, SCRATCH};
use crate::storage::{
    CheckReport, CheckStep, ObjectList, ObjectMeta, ObjectWriter, ReadStream, StorageBackend,
    StorageError, StoreIdentity, UploadPlan, Versioning,
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const LEGACY_PART: &str = ".part-";
/// A segment is a file name, and `NAME_MAX` is 255 on every file system
/// this runs on; the kernel's ENAMETOOLONG would otherwise be a fault.
use crate::storage::keys::MAX_NAME_BYTES;

pub struct FilesystemStorage {
    base_path: PathBuf,
    identity: StoreIdentity,
    write_timeout: Duration,
}

fn fault(op: &'static str, err: std::io::Error) -> StorageError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return StorageError::NotFound;
    }
    tracing::warn!(op, error = %err, "filesystem storage fault");
    StorageError::Unavailable
}

fn vanished_is_fine<T>(res: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

async fn touch(path: &Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .write(true)
            .open(&path)?
            .set_modified(SystemTime::now())
    })
    .await
    .map_err(std::io::Error::other)?
}

fn meta_of(key: String, meta: &std::fs::Metadata) -> ObjectMeta {
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    ObjectMeta {
        key,
        size: meta.len(),
        last_modified: DateTime::<Utc>::from(modified),
    }
}

#[allow(clippy::disallowed_types)]
impl FilesystemStorage {
    pub fn new(base_path: impl Into<PathBuf>, identity: StoreIdentity) -> Self {
        let base_path = base_path.into();
        std::fs::create_dir_all(&base_path).expect("failed to create storage base directory");
        let base_path = base_path
            .canonicalize()
            .expect("failed to canonicalize storage base directory");
        Self {
            base_path,
            identity,
            write_timeout: WRITE_TIMEOUT,
        }
    }

    pub fn root(&self) -> &Path {
        &self.base_path
    }

    fn scratch_dir(&self) -> PathBuf {
        self.base_path.join(SCRATCH)
    }

    fn scratch_file(&self) -> PathBuf {
        self.scratch_dir().join(uuid::Uuid::new_v4().to_string())
    }

    /// A validated key under the canonical base; a symlink planted inside
    /// the base and pointing outside is refused for existing and new files.
    fn safe_path(&self, key: &str) -> Result<PathBuf, StorageError> {
        keys::validate(key, MAX_KEY_BYTES)?;
        keys::validate_segments(key, MAX_NAME_BYTES)?;
        self.contained(key)
    }

    fn safe_prefix(&self, prefix: &str) -> Result<PathBuf, StorageError> {
        keys::validate_prefix(prefix, MAX_KEY_BYTES)?;
        keys::validate_segments(prefix, MAX_NAME_BYTES)?;
        if prefix.is_empty() {
            return Ok(self.base_path.clone());
        }
        self.contained(prefix)
    }

    fn contained(&self, rel: &str) -> Result<PathBuf, StorageError> {
        let escapes = || StorageError::InvalidPath("path escapes storage directory".to_string());
        let full_path = self.base_path.join(rel);
        let mut ancestor = full_path.as_path();
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        while ancestor.symlink_metadata().is_err() {
            match (ancestor.parent(), ancestor.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    ancestor = parent;
                }
                _ => return Err(escapes()),
            }
        }
        let canonical = ancestor.canonicalize().map_err(|_| escapes())?;
        if !canonical.starts_with(&self.base_path) {
            return Err(escapes());
        }
        let mut resolved = canonical;
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    async fn file_meta(&self, key: &str, path: &Path) -> Result<Option<ObjectMeta>, StorageError> {
        match vanished_is_fine(fs::metadata(path).await).map_err(|e| fault("stat", e))? {
            Some(meta) if meta.is_file() => Ok(Some(meta_of(key.to_string(), &meta))),
            _ => Ok(None),
        }
    }

    async fn open_writer(&self, target: PathBuf) -> Result<FsWriter, StorageError> {
        fs::create_dir_all(self.scratch_dir())
            .await
            .map_err(|e| fault("writer", e))?;
        let scratch = self.scratch_file();
        let file = fs::File::create(&scratch)
            .await
            .map_err(|e| fault("writer", e))?;
        Ok(FsWriter {
            scratch,
            target,
            file: Some(file),
            written: 0,
            committed: false,
            write_timeout: self.write_timeout,
        })
    }

    async fn copy_path(&self, from: &Path, to: PathBuf) -> Result<(), StorageError> {
        let source = fs::metadata(from).await.map_err(|e| fault("copy", e))?;
        fs::create_dir_all(self.scratch_dir())
            .await
            .map_err(|e| fault("copy", e))?;
        let scratch = self.scratch_file();
        let copied = fs::copy(from, &scratch).await;
        let copied = match copied {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&scratch).await;
                return Err(fault("copy", e));
            }
        };
        if copied != source.len() {
            let _ = fs::remove_file(&scratch).await;
            return Err(StorageError::Unavailable);
        }
        land(&scratch, &to).await?;
        let landed = fs::metadata(&to).await.map_err(|e| fault("copy", e))?;
        if landed.len() != copied {
            return Err(StorageError::Unavailable);
        }
        Ok(())
    }

    /// Removes the file, then every parent it left empty: a key space has
    /// no directories to leak.
    async fn delete_path(&self, path: &Path) -> Result<(), StorageError> {
        match fs::remove_file(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if path.is_dir() => return Ok(()),
            Err(e) => return Err(fault("delete", e)),
        }
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| *d != self.base_path && d.starts_with(&self.base_path)) {
            if fs::remove_dir(d).await.is_err() {
                break;
            }
            dir = d.parent();
        }
        Ok(())
    }

    /// Every committed file under `root`, skipping the reserved trees and
    /// legacy part files, walked lazily one directory at a time.
    fn walk(&self, root: PathBuf) -> ObjectList {
        let base = self.base_path.clone();
        let reserved = [base.join(SCRATCH), base.join(BACKEND)];
        let state = (VecDeque::from([root]), VecDeque::<ObjectMeta>::new());
        let this_base = base.clone();
        Box::pin(stream::unfold(state, move |(mut dirs, mut ready)| {
            let reserved = reserved.clone();
            let base = this_base.clone();
            async move {
                loop {
                    if let Some(meta) = ready.pop_front() {
                        return Some((Ok(meta), (dirs, ready)));
                    }
                    let dir = dirs.pop_front()?;
                    if reserved.iter().any(|r| dir.starts_with(r)) {
                        continue;
                    }
                    let meta = match vanished_is_fine(fs::metadata(&dir).await) {
                        Ok(Some(meta)) => meta,
                        Ok(None) => continue,
                        Err(e) => return Some((Err(fault("list", e)), (dirs, ready))),
                    };
                    if meta.is_file() {
                        if let Some(object) = listed(&base, &dir, &meta) {
                            ready.push_back(object);
                        }
                        continue;
                    }
                    let mut entries = match vanished_is_fine(fs::read_dir(&dir).await) {
                        Ok(Some(entries)) => entries,
                        Ok(None) => continue,
                        Err(e) => return Some((Err(fault("list", e)), (dirs, ready))),
                    };
                    let mut children = Vec::new();
                    loop {
                        match entries.next_entry().await {
                            Ok(Some(entry)) => children.push(entry.path()),
                            Ok(None) => break,
                            Err(e) => return Some((Err(fault("list", e)), (dirs, ready))),
                        }
                    }
                    children.sort();
                    dirs.extend(children);
                }
            }
        }))
    }

    async fn sweep_dir(&self, dir: &Path, cutoff: SystemTime, legacy_only: bool) -> std::io::Result<u64> {
        let mut removed = 0;
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Some(mut entries) = vanished_is_fine(fs::read_dir(&dir).await)? else {
                continue;
            };
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                let path = entry.path();
                if file_type.is_dir() {
                    if !(legacy_only && path == self.scratch_dir()) {
                        pending.push(path);
                    }
                    continue;
                }
                let eligible = !legacy_only || is_legacy_part(&path);
                if !eligible {
                    continue;
                }
                let Some(meta) = vanished_is_fine(fs::metadata(&path).await)? else {
                    continue;
                };
                if meta.modified()? < cutoff {
                    removed += vanished_is_fine(fs::remove_file(&path).await)?.is_some() as u64;
                }
            }
        }
        Ok(removed)
    }
}

fn is_legacy_part(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(LEGACY_PART))
}

fn listed(base: &Path, path: &Path, meta: &std::fs::Metadata) -> Option<ObjectMeta> {
    if is_legacy_part(path) {
        return None;
    }
    let rel = path.strip_prefix(base).ok()?;
    let key = rel.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
    Some(meta_of(key, meta))
}

/// Rename a finished scratch file over its target, then date it now: a
/// rename keeps the source's mtime, which would put a fresh object inside an
/// old window.
/// A concurrent delete may prune the parent between its creation and the
/// rename, so a vanished parent is recreated a bounded number of times.
async fn land(scratch: &Path, target: &Path) -> Result<(), StorageError> {
    let mut attempts = 0;
    loop {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| fault("commit", e))?;
        }
        match fs::rename(scratch, target).await {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && attempts < 3 => attempts += 1,
            Err(e) => {
                let _ = fs::remove_file(scratch).await;
                return Err(fault("commit", e));
            }
        }
    }
    touch(target).await.map_err(|e| fault("commit", e))
}

struct FsWriter {
    scratch: PathBuf,
    target: PathBuf,
    file: Option<fs::File>,
    written: u64,
    committed: bool,
    write_timeout: Duration,
}

#[async_trait]
impl ObjectWriter for FsWriter {
    async fn reserve(&mut self, _next_len: usize) -> Result<(), StorageError> {
        Ok(())
    }

    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError> {
        let file = self.file.as_mut().ok_or(StorageError::Unavailable)?;
        match tokio::time::timeout(self.write_timeout, file.write_all(&chunk)).await {
            Ok(Ok(())) => {
                self.written += chunk.len() as u64;
                Ok(())
            }
            Ok(Err(e)) => Err(fault("write", e)),
            Err(_) => {
                tracing::warn!("filesystem write exceeded its bound");
                Err(StorageError::Unavailable)
            }
        }
    }

    async fn commit(mut self: Box<Self>) -> Result<u64, StorageError> {
        let file = self.file.take().ok_or(StorageError::Unavailable)?;
        let scratch = self.scratch.clone();
        let target = self.target.clone();
        let size = self.written;
        self.committed = true;
        let landing = tokio::spawn(async move {
            let mut file = file;
            if let Err(e) = async {
                file.flush().await?;
                file.sync_data().await
            }
            .await
            {
                let _ = fs::remove_file(&scratch).await;
                return Err(fault("commit", e));
            }
            drop(file);
            land(&scratch, &target).await.map(|()| size)
        });
        landing.await.map_err(|_| StorageError::Unavailable)?
    }
}

impl Drop for FsWriter {
    // Synchronous: Drop cannot await and a spawned task would leak at shutdown.
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.scratch);
        }
    }
}

#[async_trait]
#[allow(clippy::disallowed_types)]
impl StorageBackend for FilesystemStorage {
    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        let path = self.safe_path(key)?;
        match fs::read(&path).await {
            Ok(data) => Ok(Bytes::from(data)),
            Err(_) if path.is_dir() => Err(StorageError::NotFound),
            Err(e) => Err(fault("get", e)),
        }
    }

    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        let target = self.safe_path(key)?;
        Ok(Box::new(self.open_writer(target).await?))
    }

    async fn read_stream(&self, key: &str) -> Result<ReadStream, StorageError> {
        let path = self.safe_path(key)?;
        let file = fs::File::open(&path)
            .await
            .map_err(|e| fault("read", e))?;
        let meta = file.metadata().await.map_err(|e| fault("read", e))?;
        if !meta.is_file() {
            return Err(StorageError::NotFound);
        }
        Ok(ReadStream {
            total: meta.len(),
            body: Box::pin(file),
        })
    }

    async fn copy_object(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from = self.safe_path(from)?;
        let to = self.safe_path(to)?;
        self.copy_path(&from, to).await
    }

    async fn relocate(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_path = self.safe_path(from)?;
        let to_path = self.safe_path(to)?;
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| fault("relocate", e))?;
        }
        match fs::rename(&from_path, &to_path).await {
            Ok(()) => touch(&to_path).await.map_err(|e| fault("relocate", e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match self.file_meta(to, &to_path).await? {
                    Some(_) => Ok(()),
                    None => Err(StorageError::NotFound),
                }
            }
            Err(_) => self.copy_path(&from_path, to_path).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        self.stat(key).await
    }

    async fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        let path = self.safe_path(key)?;
        self.file_meta(key, &path).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = self.safe_path(key)?;
        self.delete_path(&path).await
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), StorageError> {
        let paths = keys
            .iter()
            .map(|k| self.safe_path(k))
            .collect::<Result<Vec<_>, _>>()?;
        for path in &paths {
            self.delete_path(path).await?;
        }
        Ok(())
    }

    fn list(&self, prefix: &str) -> ObjectList {
        match self.safe_prefix(prefix) {
            Ok(root) => self.walk(root),
            Err(e) => Box::pin(stream::once(async move { Err(e) })),
        }
    }

    async fn sweep_abandoned(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StorageError> {
        let cutoff = SystemTime::from(now) - older_than;
        let scratch = self
            .sweep_dir(&self.scratch_dir(), cutoff, false)
            .await
            .map_err(|e| fault("sweep", e))?;
        let legacy = self
            .sweep_dir(&self.base_path, cutoff, true)
            .await
            .map_err(|e| fault("sweep", e))?;
        Ok(scratch + legacy)
    }

    /// A directory keeps nothing an overwrite replaced.
    async fn versioning(&self) -> Result<Versioning, StorageError> {
        Ok(Versioning::NotKept)
    }

    async fn probe(&self) -> Result<(), StorageError> {
        let health = self.base_path.join(BACKEND).join("health");
        if fs::metadata(&health).await.is_err() {
            let writer = self.open_writer(health.clone()).await?;
            let mut writer: Box<dyn ObjectWriter> = Box::new(writer);
            writer.write(Bytes::from_static(b"ok")).await?;
            writer.commit().await?;
        }
        fs::read(&health).await.map_err(|e| fault("probe", e))?;
        Ok(())
    }

    fn key_budget(&self) -> keys::KeyBudget {
        keys::KeyBudget {
            key: MAX_KEY_BYTES,
            segment: MAX_NAME_BYTES,
        }
    }

    fn upload_plan(&self) -> UploadPlan {
        UploadPlan {
            max_object_bytes: 4 * 1024 * 1024 * 1024,
            min_chunk_bytes: 0,
            completion_bound: Duration::from_secs(300),
            delete_bound: Duration::from_secs(60),
            delete_batch: 1000,
        }
    }

    async fn self_check(&self) -> CheckReport {
        let root = self
            .base_path
            .join(BACKEND)
            .join("self-check")
            .join(uuid::Uuid::new_v4().to_string());
        let a = root.join("a");
        let b = root.join("b");
        let mut steps = Vec::new();
        let mut step = |operation: &'static str, outcome: Result<(), StorageError>| {
            steps.push(CheckStep {
                operation,
                outcome: outcome.map_err(|e| e.to_string()),
            });
        };
        let written = async {
            let mut w: Box<dyn ObjectWriter> = Box::new(self.open_writer(a.clone()).await?);
            w.reserve(4).await?;
            w.write(Bytes::from_static(b"self")).await?;
            w.commit().await.map(|_| ())
        }
        .await;
        step("writer", written);
        step(
            "read",
            fs::read(&a)
                .await
                .map_err(|e| fault("read", e))
                .and_then(|d| if d == b"self" { Ok(()) } else { Err(StorageError::Unavailable) }),
        );
        step("copy_object", self.copy_path(&a, b.clone()).await);
        let listed = {
            use futures_util::StreamExt;
            let found: Vec<_> = self.walk(root.clone()).collect().await;
            if found.is_empty() {
                Ok(())
            } else {
                Err(StorageError::Other("the reserved tree was listed".into()))
            }
        };
        step("list", listed);
        step("delete", self.delete_path(&b).await);
        let _ = fs::remove_dir_all(&root).await;
        CheckReport { steps }
    }

    fn identity(&self) -> StoreIdentity {
        self.identity.clone()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests;
