//! Advisory locks over files beside the database and inside a backup
//! target: taken atomically, held by an open descriptor, released when the
//! guard drops or its process dies.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::bail;
use rustix::fs::{flock, FlockOperation};

/// How a door opens the database: every door but the restore shares it.
#[derive(Debug, Clone, Copy)]
pub enum Lock<'a> {
    Shared,
    /// The restore door, with the snapshot it restores.
    Exclusive(&'a Path),
}

/// Holds `{db}.lock`; the lock lives exactly as long as this value.
#[derive(Debug)]
pub struct RestoreLock {
    _file: File,
}

/// Holds `<to>/.backup.lock` for one run into `to`.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
}

pub fn lock_path(db_path: &Path) -> PathBuf {
    beside(db_path, ".lock")
}

/// Written by a restore before its storage phase, removed by its success.
pub fn marker_path(db_path: &Path) -> PathBuf {
    beside(db_path, ".restore-in-progress")
}

fn beside(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn open_lock_file(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new().create(true).truncate(false).write(true).open(path)?)
}

/// The refusal every door that opens the database shares. `{db}.lock` is
/// created by whichever door comes first and never removed; a contended
/// acquire is the whole refusal. The marker left by an interrupted restore
/// refuses every shared door, and lets the restore door resume only the
/// snapshot it names.
pub fn restore_lock_guard(db_path: &Path, lock: Lock<'_>) -> anyhow::Result<RestoreLock> {
    let file = open_lock_file(&lock_path(db_path))?;
    let operation = match lock {
        Lock::Shared => FlockOperation::NonBlockingLockShared,
        Lock::Exclusive(_) => FlockOperation::NonBlockingLockExclusive,
    };
    if flock(&file, operation).is_err() {
        match lock {
            Lock::Shared => bail!(
                "a restore is running on {}: wait for it to finish",
                db_path.display()
            ),
            Lock::Exclusive(_) => bail!(
                "{} is open by a server or another command: stop it (scale the deployment to 0) before restoring",
                db_path.display()
            ),
        }
    }
    let marker = marker_path(db_path);
    if let Ok(recorded) = std::fs::read_to_string(&marker) {
        let recorded = PathBuf::from(recorded.trim());
        match lock {
            Lock::Shared => bail!(
                "an interrupted restore left {}: finish it with `opencargo restore --from {}` (deleting the file is unsupported)",
                marker.display(),
                recorded.display()
            ),
            Lock::Exclusive(from) if !same_path(&recorded, from) => bail!(
                "an interrupted restore of {} is not finished; restoring {} over it is refused, whatever the flags",
                recorded.display(),
                from.display()
            ),
            Lock::Exclusive(_) => {}
        }
    }
    Ok(RestoreLock { _file: file })
}

/// Serialises every run into one backup target.
pub fn backup_dir_lock(to: &Path) -> anyhow::Result<DirLock> {
    std::fs::create_dir_all(to)?;
    let file = open_lock_file(&to.join(".backup.lock"))?;
    if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
        bail!("another backup is running into {}", to.display());
    }
    Ok(DirLock { _file: file })
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_doors_coexist_and_exclude_the_restore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("db/opencargo.db");
        let snapshot = tmp.path().join("snap");
        let a = restore_lock_guard(&db, Lock::Shared).unwrap();
        let b = restore_lock_guard(&db, Lock::Shared).unwrap();
        assert!(restore_lock_guard(&db, Lock::Exclusive(&snapshot)).is_err());
        drop((a, b));
        let restore = restore_lock_guard(&db, Lock::Exclusive(&snapshot)).unwrap();
        assert!(restore_lock_guard(&db, Lock::Shared).is_err());
        drop(restore);
        assert!(lock_path(&db).exists(), "the lock file is permanent");
        restore_lock_guard(&db, Lock::Shared).unwrap();
    }

    #[test]
    fn the_marker_refuses_shared_doors_and_another_snapshot_but_resumes_its_own() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("opencargo.db");
        let (mine, other) = (tmp.path().join("mine"), tmp.path().join("other"));
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(marker_path(&db), mine.display().to_string()).unwrap();
        let refused = restore_lock_guard(&db, Lock::Shared).unwrap_err().to_string();
        assert!(refused.contains("restore-in-progress") && refused.contains("mine"), "{refused}");
        assert!(restore_lock_guard(&db, Lock::Exclusive(&other)).is_err());
        restore_lock_guard(&db, Lock::Exclusive(&mine)).unwrap();
    }

    #[test]
    fn two_runs_into_one_target_serialise() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = backup_dir_lock(tmp.path()).unwrap();
        assert!(backup_dir_lock(tmp.path()).is_err());
        drop(first);
        backup_dir_lock(tmp.path()).unwrap();
    }
}
