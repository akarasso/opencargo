use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::*;
use crate::error::StoreError;
use crate::ports::backup::Checkpoint;

/// A database whose checkpoint is held off `busy_for` times.
struct Db {
    busy_for: usize,
    checkpoints: AtomicUsize,
    snapshots: AtomicUsize,
}

#[async_trait]
impl DatabaseBackup for Db {
    async fn snapshot_into(&self, dest: &Path) -> Result<(), StoreError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        std::fs::write(dest, b"snapshot").map_err(|e| StoreError::Other(Box::new(e)))
    }

    async fn checkpoint(&self) -> Result<Checkpoint, StoreError> {
        let n = self.checkpoints.fetch_add(1, Ordering::SeqCst);
        Ok(Checkpoint {
            busy: n < self.busy_for,
            log_pages: 10,
            checkpointed_pages: 3,
        })
    }

    async fn size(&self) -> Result<u64, StoreError> {
        Ok(8)
    }
}

struct Files;

#[async_trait]
impl DatabaseFiles for Files {
    async fn verify(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }

    async fn replace(&self, _: &Path, _: &Path) -> Result<(), StoreError> {
        Ok(())
    }
}

#[derive(Default)]
struct State(Mutex<HashMap<String, String>>);

#[async_trait]
impl ServerStateStore for State {
    async fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(self.0.lock().unwrap().get(name).cloned())
    }

    async fn set(&self, name: &str, value: &str, _: DateTime<Utc>) -> Result<(), StoreError> {
        self.0.lock().unwrap().insert(name.to_string(), value.to_string());
        Ok(())
    }
}

struct Wall;

impl Clock for Wall {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

fn backup(tmp: &tempfile::TempDir, busy_for: usize) -> (Backup, Arc<Db>, Arc<State>) {
    let db = Arc::new(Db {
        busy_for,
        checkpoints: AtomicUsize::new(0),
        snapshots: AtomicUsize::new(0),
    });
    let state = Arc::new(State::default());
    let backup = Backup {
        database: db.clone(),
        files: Arc::new(Files),
        storage: crate::server::filesystem(tmp.path().join("storage")),
        state: state.clone(),
        clock: Arc::new(Wall),
        sink: None,
    };
    (backup, db, state)
}

fn plan(tmp: &tempfile::TempDir) -> BackupPlan {
    BackupPlan {
        to: tmp.path().join("to"),
        storage: false,
        force: false,
        keep: 3,
        space: Arc::new(StatvfsProbe),
    }
}

#[tokio::test(start_paused = true)]
async fn a_busy_checkpoint_is_retried_and_recorded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (held_off, db, state) = backup(&tmp, usize::MAX);
    let snapshot = held_off.run(&plan(&tmp)).await.expect("a busy checkpoint never fails the run");
    assert!(!snapshot.wal_truncated);
    assert_eq!(snapshot.checkpoint_attempts, CHECKPOINT_BACKOFF.len() + 1);
    assert_eq!(db.checkpoints.load(Ordering::SeqCst), CHECKPOINT_BACKOFF.len() + 1);
    assert_eq!(state.get(LAST_BACKUP_WAL).await.unwrap().as_deref(), Some("busy"));
    assert!(state.get(LAST_BACKUP_AT).await.unwrap().is_some());

    let (brief, _, state) = backup(&tmp, 2);
    let snapshot = brief.run(&plan(&tmp)).await.unwrap();
    assert!(snapshot.wal_truncated);
    assert_eq!(snapshot.checkpoint_attempts, 3);
    assert_eq!(state.get(LAST_BACKUP_WAL).await.unwrap().as_deref(), Some("truncated"));
}

#[tokio::test(start_paused = true)]
async fn the_scheduler_runs_only_for_the_lease_holder() {
    let every = Duration::from_secs(3600);
    let at = schedule::parse_at("00:00").unwrap();
    for (held, want) in [(false, 0), (true, 1)] {
        let tmp = tempfile::TempDir::new().unwrap();
        let (backup, db, _) = backup(&tmp, 0);
        let lease = crate::app::lease::LeaseHandle::detached(held);
        let task = tokio::spawn(schedule::start_backup_schedule(backup, Arc::new(plan(&tmp)), every, at, lease));
        tokio::time::sleep(every + Duration::from_secs(1)).await;
        task.abort();
        let ran = db.snapshots.load(Ordering::SeqCst);
        assert!(if want == 0 { ran == 0 } else { ran >= 1 }, "held = {held}: {ran} runs");
    }
}
