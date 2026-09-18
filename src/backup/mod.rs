//! Backup, check and the storage half of a restore: database first, then
//! storage, so drift inside a snapshot is an orphan object and never a row
//! without one. A snapshot is a local directory; its manifest is written
//! last.

pub mod lock;
pub mod manifest;
pub mod schedule;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use bytes::Bytes;
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use crate::ports::backup::{DatabaseBackup, DatabaseFiles};
use crate::ports::clock::Clock;
use crate::ports::leases::ServerStateStore;
use crate::storage::StorageBackend;
use manifest::{KeyLine, Manifest, DATABASE, FORMAT_VERSION, KEYS, MANIFEST, STORAGE_DIR};

pub const LAST_BACKUP_AT: &str = "last_backup_at";
pub const LAST_BACKUP_TO: &str = "last_backup_to";
pub const LAST_BACKUP_WAL: &str = "last_backup_wal";

const SNAPSHOT_PREFIX: &str = "opencargo-";
const CHUNK: usize = 1 << 20;
/// Pauses between checkpoint attempts a reader held off: about ten seconds.
const CHECKPOINT_BACKOFF: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(5),
];

/// Free space where a snapshot goes; a seam so the refusal is provable
/// without a small filesystem.
pub trait SpaceProbe: Send + Sync {
    fn free_bytes(&self, path: &Path) -> io::Result<u64>;
}

pub struct StatvfsProbe;

impl SpaceProbe for StatvfsProbe {
    fn free_bytes(&self, path: &Path) -> io::Result<u64> {
        let stat = rustix::fs::statvfs(path)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
}

pub struct BackupPlan {
    /// A local directory.
    pub to: PathBuf,
    pub storage: bool,
    pub force: bool,
    pub keep: usize,
    pub space: Arc<dyn SpaceProbe>,
}

/// Where a finished snapshot is copied off-box. Built by the run that uses
/// it, so a sink fault fails that run and never a boot.
#[async_trait::async_trait]
pub trait SnapshotSink: Send + Sync {
    async fn upload(&self, dir: &Path) -> anyhow::Result<()>;
}

/// A backup over the ports it reads.
#[derive(Clone)]
pub struct Backup {
    pub database: Arc<dyn DatabaseBackup>,
    pub files: Arc<dyn DatabaseFiles>,
    pub storage: Arc<dyn StorageBackend>,
    pub state: Arc<dyn ServerStateStore>,
    pub clock: Arc<dyn Clock>,
    pub sink: Option<Arc<dyn SnapshotSink>>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// Whether the post-run checkpoint truncated the WAL.
    pub wal_truncated: bool,
    pub checkpoint_attempts: usize,
}

impl Backup {
    /// One run into `plan.to`, serialised with every other run into it.
    pub async fn run(&self, plan: &BackupPlan) -> anyhow::Result<Snapshot> {
        let _target = lock::backup_dir_lock(&plan.to)?;
        let to = plan.to.canonicalize()?;
        self.record(LAST_BACKUP_TO, &to.display().to_string()).await;
        let reclaimed = reclaim_incomplete(&to)?;
        if reclaimed > 0 {
            info!(reclaimed, "interrupted snapshots reclaimed");
        }

        let db_bytes = self.database.size().await?;
        let storage_bytes = if plan.storage { self.stored_bytes().await? } else { 0 };
        let estimate = db_bytes + storage_bytes;
        let free = plan.space.free_bytes(&to)?;
        if free < estimate {
            bail!(
                "not enough space in {}: the snapshot needs about {estimate} bytes, {free} are free, {} short",
                to.display(),
                estimate - free
            );
        }

        let taken_at = self.clock.now();
        let dir = to.join(format!("{SNAPSHOT_PREFIX}{}", taken_at.format("%Y%m%dT%H%M%S%.3fZ")));
        if dir.exists() {
            if !plan.force && std::fs::read_dir(&dir)?.next().is_some() {
                bail!("{} is not empty: pass --force to overwrite it", dir.display());
            }
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;

        let db_file = dir.join(DATABASE);
        self.database.snapshot_into(&db_file).await?;
        self.files
            .verify(&db_file)
            .await
            .map_err(|problem| anyhow::anyhow!("the database copy failed its integrity check: {problem}"))?;
        let db_sha256 = sha256_file(&db_file).await?;
        let (wal_truncated, checkpoint_attempts) = self.checkpoint().await;

        let (storage_keys, copied_bytes) = if plan.storage {
            copy_storage(self.storage.as_ref(), &dir).await?
        } else {
            tokio::fs::write(dir.join(KEYS), b"").await?;
            (0, 0)
        };

        let manifest = Manifest {
            version: FORMAT_VERSION,
            taken_at,
            db_sha256,
            storage: plan.storage,
            storage_keys,
            storage_bytes: copied_bytes,
        };
        let staged = dir.join(".manifest.json");
        tokio::fs::write(&staged, serde_json::to_vec_pretty(&manifest)?).await?;
        tokio::fs::rename(&staged, dir.join(MANIFEST)).await?;
        if let Some(sink) = &self.sink {
            sink.upload(&dir).await?;
        }
        prune(&to, plan.keep.max(1))?;
        self.record(LAST_BACKUP_AT, &taken_at.to_rfc3339()).await;
        info!(snapshot = %dir.display(), storage_keys, "backup complete");
        Ok(Snapshot {
            dir,
            manifest,
            wal_truncated,
            checkpoint_attempts,
        })
    }

    async fn stored_bytes(&self) -> anyhow::Result<u64> {
        let mut total = 0u64;
        let mut objects = self.storage.list("");
        while let Some(meta) = objects.try_next().await? {
            total += meta.size;
        }
        Ok(total)
    }

    /// The copy held a read transaction the whole time, so the WAL could not
    /// be checkpointed; a busy answer is retried, then reported.
    async fn checkpoint(&self) -> (bool, usize) {
        let mut attempts = 0;
        let mut outcome = None;
        for pause in CHECKPOINT_BACKOFF.iter().map(Some).chain([None]) {
            attempts += 1;
            match self.database.checkpoint().await {
                Ok(done) if !done.busy => {
                    outcome = Some(done);
                    break;
                }
                Ok(done) => outcome = Some(done),
                Err(e) => warn!(error = %e, "checkpoint after backup failed"),
            }
            if let Some(pause) = pause {
                tokio::time::sleep(*pause).await;
            }
        }
        let truncated = outcome.is_some_and(|c| !c.busy);
        if !truncated {
            warn!(
                log_pages = outcome.map(|c| c.log_pages),
                checkpointed_pages = outcome.map(|c| c.checkpointed_pages),
                "the WAL could not be truncated after the backup"
            );
        }
        self.record(LAST_BACKUP_WAL, if truncated { "truncated" } else { "busy" }).await;
        (truncated, attempts)
    }

    async fn record(&self, name: &str, value: &str) {
        if let Err(e) = self.state.set(name, value, self.clock.now()).await {
            warn!(error = %e, name, "backup state not recorded");
        }
    }
}

/// Every object of `storage` into `dir/storage/`, with its line in
/// `keys.sha256` appended as it is copied.
async fn copy_storage(storage: &dyn StorageBackend, dir: &Path) -> anyhow::Result<(u64, u64)> {
    let mut index = tokio::fs::File::create(dir.join(KEYS)).await?;
    let (mut count, mut total) = (0u64, 0u64);
    let mut objects = storage.list("");
    while let Some(meta) = objects.try_next().await? {
        let target = object_path(dir, &meta.key)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut body = storage.read_stream(&meta.key).await?.body;
        let mut out = tokio::fs::File::create(&target).await?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = body.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).await?;
            bytes += n as u64;
        }
        out.flush().await?;
        let line = KeyLine {
            sha256: hex(&hasher.finalize()),
            bytes,
            key: meta.key,
        };
        index.write_all(line.render().as_bytes()).await?;
        count += 1;
        total += bytes;
    }
    index.flush().await?;
    Ok((count, total))
}

/// `key` under `dir/storage/`, refusing anything that would leave it.
fn object_path(dir: &Path, key: &str) -> anyhow::Result<PathBuf> {
    if key.split('/').any(|s| s.is_empty() || s == "." || s == "..") || key.starts_with('/') {
        bail!("refusing the storage key {key:?}");
    }
    Ok(dir.join(STORAGE_DIR).join(key))
}

/// The snapshot's manifest, refused when a newer opencargo wrote it.
pub fn read_manifest(dir: &Path) -> anyhow::Result<Manifest> {
    let path = dir.join(MANIFEST);
    let raw = std::fs::read(&path).with_context(|| format!("{} has no manifest: an interrupted run", dir.display()))?;
    let manifest: Manifest = serde_json::from_slice(&raw).with_context(|| format!("{} is not a manifest", path.display()))?;
    if manifest.version > FORMAT_VERSION {
        bail!(
            "{} is manifest version {}, this opencargo reads up to version {FORMAT_VERSION}",
            path.display(),
            manifest.version
        );
    }
    Ok(manifest)
}

/// Every piece of evidence a snapshot carries, without restoring it.
pub async fn check(dir: &Path, files: &dyn DatabaseFiles) -> anyhow::Result<Manifest> {
    let manifest = read_manifest(dir)?;
    let db_file = dir.join(DATABASE);
    if sha256_file(&db_file).await? != manifest.db_sha256 {
        bail!("{} does not match the sha256 its manifest records", db_file.display());
    }
    files
        .verify(&db_file)
        .await
        .map_err(|problem| anyhow::anyhow!("{} fails its integrity check: {problem}", db_file.display()))?;
    let mut lines = 0u64;
    for line in key_lines(dir).await? {
        let path = object_path(dir, &line.key)?;
        let (sha, bytes) = sha256_and_len(&path).await.with_context(|| format!("key {} is missing", line.key))?;
        if sha != line.sha256 || bytes != line.bytes {
            bail!("key {} does not match keys.sha256", line.key);
        }
        lines += 1;
    }
    if lines != manifest.storage_keys {
        bail!("keys.sha256 lists {lines} objects, the manifest {}", manifest.storage_keys);
    }
    Ok(manifest)
}

async fn key_lines(dir: &Path) -> anyhow::Result<Vec<KeyLine>> {
    let raw = tokio::fs::read_to_string(dir.join(KEYS)).await?;
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| KeyLine::parse(l).with_context(|| format!("unreadable keys.sha256 line: {l:?}")))
        .collect()
}

/// The storage half of a restore: every object of the snapshot written back
/// under its key, verified against `keys.sha256` before it becomes visible.
pub async fn restore_storage(dir: &Path, storage: &dyn StorageBackend) -> anyhow::Result<u64> {
    let mut restored = 0;
    for line in key_lines(dir).await? {
        let path = object_path(dir, &line.key)?;
        let mut file = tokio::fs::File::open(&path).await?;
        let mut writer = storage.writer(&line.key).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.reserve(n).await?;
            writer.write(Bytes::copy_from_slice(&buf[..n])).await?;
        }
        if hex(&hasher.finalize()) != line.sha256 {
            bail!("key {} changed since the snapshot was checked", line.key);
        }
        writer.commit().await?;
        restored += 1;
    }
    Ok(restored)
}

/// The snapshot directories under `to` whose run never finished, and their
/// bytes.
pub fn incomplete_snapshots(to: &Path) -> io::Result<(usize, u64)> {
    let mut count = 0;
    let mut bytes = 0;
    for dir in snapshot_dirs(to)? {
        if !dir.join(MANIFEST).exists() {
            count += 1;
            bytes += tree_bytes(&dir)?;
        }
    }
    Ok((count, bytes))
}

fn snapshot_dirs(to: &Path) -> io::Result<Vec<PathBuf>> {
    if !to.exists() {
        return Ok(Vec::new());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(to)?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_string_lossy().starts_with(SNAPSHOT_PREFIX))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn tree_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        total += if kind.is_dir() { tree_bytes(&entry.path())? } else { entry.metadata()?.len() };
    }
    Ok(total)
}

/// Safe only under `to`'s backup lock: nobody else can be writing one.
fn reclaim_incomplete(to: &Path) -> io::Result<usize> {
    let mut reclaimed = 0;
    for dir in snapshot_dirs(to)? {
        if !dir.join(MANIFEST).exists() {
            std::fs::remove_dir_all(&dir)?;
            reclaimed += 1;
        }
    }
    Ok(reclaimed)
}

/// Keeps the newest `keep` complete snapshots.
fn prune(to: &Path, keep: usize) -> io::Result<()> {
    let complete: Vec<PathBuf> = snapshot_dirs(to)?
        .into_iter()
        .filter(|d| d.join(MANIFEST).exists())
        .collect();
    let excess = complete.len().saturating_sub(keep);
    for dir in &complete[..excess] {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

async fn sha256_file(path: &Path) -> io::Result<String> {
    Ok(sha256_and_len(path).await?.0)
}

async fn sha256_and_len(path: &Path) -> io::Result<(String, u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((hex(&hasher.finalize()), len))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The snapshot's database file, for the swap.
pub fn database_file(dir: &Path) -> PathBuf {
    dir.join(DATABASE)
}

/// A finished snapshot into an off-box store, under its directory name:
/// every file, the manifest last, so a sink never holds a manifest whose
/// files are missing.
pub async fn upload(dir: &Path, sink: &dyn StorageBackend) -> anyhow::Result<u64> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("a snapshot directory has a UTF-8 name")?
        .to_string();
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by_key(|rel| rel == MANIFEST);
    let mut uploaded = 0;
    for rel in files {
        let mut file = tokio::fs::File::open(dir.join(&rel)).await?;
        let mut writer = sink.writer(&format!("{name}/{rel}")).await?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.reserve(n).await?;
            writer.write(Bytes::copy_from_slice(&buf[..n])).await?;
        }
        writer.commit().await?;
        uploaded += 1;
    }
    Ok(uploaded)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            if !rel.starts_with('.') {
                out.push(rel);
            }
        }
    }
    Ok(())
}
