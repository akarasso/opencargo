//! Parsed upstream pages, keyed by the cache row they were parsed from, so
//! a page is parsed once per fetch however many files it serves. Bounded by
//! entries and by weight; a stale body is never memoized.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::parse::UpstreamPage;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub row: i64,
    pub fetched_at: DateTime<Utc>,
    pub digest: Option<String>,
}

struct Inner {
    pages: HashMap<PageKey, Arc<UpstreamPage>>,
    order: VecDeque<PageKey>,
    weight: usize,
}

pub struct PageMemo {
    max_entries: usize,
    max_weight: usize,
    inner: Mutex<Inner>,
}

impl PageMemo {
    pub fn new(max_entries: usize, max_weight: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            max_weight,
            inner: Mutex::new(Inner {
                pages: HashMap::new(),
                order: VecDeque::new(),
                weight: 0,
            }),
        }
    }

    pub fn get(&self, key: &PageKey) -> Option<Arc<UpstreamPage>> {
        let mut inner = self.inner.lock().expect("page memo poisoned");
        let page = inner.pages.get(key).cloned()?;
        if let Some(at) = inner.order.iter().position(|k| k == key) {
            let k = inner.order.remove(at).expect("position is in range");
            inner.order.push_back(k);
        }
        Some(page)
    }

    pub fn insert(&self, key: PageKey, page: Arc<UpstreamPage>) {
        let weight = page.weight();
        if weight > self.max_weight {
            return;
        }
        let mut inner = self.inner.lock().expect("page memo poisoned");
        if let Some(old) = inner.pages.insert(key.clone(), page) {
            inner.weight -= old.weight();
            inner.order.retain(|k| k != &key);
        }
        inner.weight += weight;
        inner.order.push_back(key);
        while inner.pages.len() > self.max_entries || inner.weight > self.max_weight {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(gone) = inner.pages.remove(&oldest) {
                inner.weight -= gone.weight();
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().pages.len()
    }
}

impl Default for PageMemo {
    fn default() -> Self {
        Self::new(1024, 64 * 1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::pypi::parse::UpstreamFile;
    use crate::registry::pypi::simple::Yanked;

    fn key(row: i64) -> PageKey {
        PageKey {
            row,
            fetched_at: DateTime::UNIX_EPOCH,
            digest: None,
        }
    }

    fn page(files: usize) -> Arc<UpstreamPage> {
        let file = UpstreamFile {
            filename: "a".into(),
            url: url::Url::parse("https://x/a").unwrap(),
            sha256: None,
            requires_python: None,
            yanked: Yanked::No,
            core_metadata: None,
        };
        Arc::new(UpstreamPage {
            files: vec![file; files],
        })
    }

    #[test]
    fn bounded_by_entries_and_by_weight() {
        let one = page(1).weight();
        let memo = PageMemo::new(2, one * 3);
        memo.insert(key(1), page(1));
        memo.insert(key(2), page(1));
        assert!(memo.get(&key(1)).is_some(), "touched");
        memo.insert(key(3), page(1));
        assert_eq!(memo.len(), 2);
        assert!(memo.get(&key(2)).is_none(), "the least recently used went");
        memo.insert(key(4), page(3));
        assert!(memo.get(&key(4)).is_some());
        assert_eq!(memo.len(), 1, "weight evicts too");
        memo.insert(key(5), page(4));
        assert!(memo.get(&key(5)).is_none(), "an entry heavier than the memo is not kept");
    }
}
