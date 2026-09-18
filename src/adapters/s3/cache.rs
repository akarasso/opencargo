//! The positive-existence cache behind `head`: filled by a successful
//! `head`, a committed write or a verified copy, emptied by this process's
//! deletes, and never read nor filled by `stat`. Sound only while one
//! process writes the prefix.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::storage::ObjectMeta;

type Entries = HashMap<String, (u64, DateTime<Utc>)>;

pub struct ExistsCache {
    capacity: usize,
    inner: Mutex<(Entries, VecDeque<String>)>,
}

impl ExistsCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }

    pub fn get(&self, key: &str) -> Option<ObjectMeta> {
        let inner = self.inner.lock().unwrap();
        inner.0.get(key).map(|(size, at)| ObjectMeta {
            key: key.to_string(),
            size: *size,
            last_modified: *at,
        })
    }

    pub fn fill(&self, meta: &ObjectMeta) {
        let mut inner = self.inner.lock().unwrap();
        let (map, order) = &mut *inner;
        if map
            .insert(meta.key.clone(), (meta.size, meta.last_modified))
            .is_none()
        {
            order.push_back(meta.key.clone());
        }
        while map.len() > self.capacity {
            match order.pop_front() {
                Some(oldest) => {
                    map.remove(&oldest);
                }
                None => break,
            }
        }
    }

    pub fn forget(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.0.remove(key).is_some() {
            inner.1.retain(|k| k != key);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
