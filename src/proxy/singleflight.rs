use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::OwnedMutexGuard;

type Locks = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// Per-key lock with a wait deadline: the leader downloads, followers wait
/// for it and then re-read the cache row; a follower past the deadline
/// proceeds unlocked.
#[derive(Default)]
pub struct Singleflight {
    locks: Locks,
}

pub struct Guard {
    key: String,
    locks: Locks,
    _permit: OwnedMutexGuard<()>,
}

impl Singleflight {
    pub async fn acquire(&self, key: &str, wait: Duration) -> Option<Guard> {
        let lock = self
            .locks
            .lock()
            .expect("singleflight map poisoned")
            .entry(key.to_string())
            .or_default()
            .clone();
        let permit = tokio::time::timeout(wait, lock.lock_owned()).await.ok()?;
        Some(Guard {
            key: key.to_string(),
            locks: self.locks.clone(),
            _permit: permit,
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut locks = self.locks.lock().expect("singleflight map poisoned");
        // The map and our own permit hold one Arc each; any third holder is a waiter.
        if locks
            .get(&self.key)
            .is_some_and(|l| Arc::strong_count(l) <= 2)
        {
            locks.remove(&self.key);
        }
    }
}
