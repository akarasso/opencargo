//! `MergeCache` (NuGet decision 11): what was derived from members'
//! documents, kept while its sources have not moved. A hosted source is
//! validated by its package's version stamp, anything read from an upstream
//! document by an age cap. Concurrent callers of one key compute once, and
//! wait for that computation no longer than a bound.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// What a stored answer was derived from: the stamps of its hosted sources,
/// in view order, and whether an upstream document was one of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sources {
    pub stamps: Vec<String>,
    pub upstream: bool,
}

struct Stored<V> {
    value: V,
    sources: Sources,
    at: Instant,
}

type Slot<V> = Arc<tokio::sync::Mutex<Option<Stored<V>>>>;

struct Table<K, V> {
    slots: HashMap<K, Slot<V>>,
    kept: HashMap<K, (usize, u64)>,
    weight: usize,
    tick: u64,
}

pub struct Memo<K, V> {
    table: Mutex<Table<K, V>>,
    budget: usize,
    max_age: Duration,
    wait: Duration,
    weigh: fn(&V) -> usize,
}

const OVERHEAD: usize = 256;
const DOCUMENT_BUDGET: usize = 64 * 1024 * 1024;
const UPSTREAM_MAX_AGE: Duration = Duration::from_secs(60);
const WAIT: Duration = Duration::from_secs(10);

/// A rendered document of one repository, for one permission view.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocKey {
    pub repo: i64,
    pub repo_name: String,
    pub id: String,
    pub view: Vec<i64>,
    pub doc: String,
}

pub type Documents = Memo<DocKey, bytes::Bytes>;

pub fn documents() -> Documents {
    Memo::new(DOCUMENT_BUDGET, UPSTREAM_MAX_AGE, WAIT, bytes::Bytes::len)
}

/// Upstream documents parsed once per body: the key is the body's sha256,
/// so an entry never outlives the bytes it came from.
/// Weighed by the size of the body it was parsed from.
pub type Parsed = Memo<String, (Arc<serde_json::Value>, usize)>;

pub fn parsed() -> &'static Parsed {
    static PARSED: std::sync::OnceLock<Parsed> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| Memo::new(DOCUMENT_BUDGET, Duration::from_secs(600), WAIT, |(_, n)| *n))
}

impl<K: Eq + Hash + Clone, V: Clone> Memo<K, V> {
    pub fn new(budget: usize, max_age: Duration, wait: Duration, weigh: fn(&V) -> usize) -> Self {
        Self {
            table: Mutex::new(Table {
                slots: HashMap::new(),
                kept: HashMap::new(),
                weight: 0,
                tick: 0,
            }),
            budget,
            max_age,
            wait,
            weigh,
        }
    }

    fn fresh(&self, stored: &Stored<V>, sources: &Sources) -> bool {
        stored.sources == *sources && (!sources.upstream || stored.at.elapsed() < self.max_age)
    }

    fn slot(&self, key: &K) -> Slot<V> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.slots.entry(key.clone()).or_default().clone()
    }

    fn forget(&self, key: &K, slot: &Slot<V>) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if table.slots.get(key).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            table.slots.remove(key);
        }
        if let Some((w, _)) = table.kept.remove(key) {
            table.weight -= w;
        }
    }

    fn keep(&self, key: &K, weight: usize) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.tick += 1;
        let tick = table.tick;
        if let Some((w, _)) = table.kept.insert(key.clone(), (weight, tick)) {
            table.weight -= w;
        }
        table.weight += weight;
        while table.weight > self.budget {
            let Some(oldest) = table
                .kept
                .iter()
                .filter(|(k, _)| *k != key)
                .min_by_key(|(_, (_, tick))| *tick)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((w, _)) = table.kept.remove(&oldest) {
                table.weight -= w;
            }
            table.slots.remove(&oldest);
        }
    }

    /// The stored value when its sources are unchanged, else `compute`'s,
    /// stored when it says so. A caller that cannot join an in-flight
    /// computation within the wait computes on its own and stores nothing.
    pub async fn get_or_compute<E, F, Fut>(&self, key: K, sources: Sources, compute: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(V, bool), E>>,
    {
        let slot = self.slot(&key);
        let Ok(mut guard) = tokio::time::timeout(self.wait, slot.clone().lock_owned()).await else {
            return compute().await.map(|(value, _)| value);
        };
        if let Some(stored) = guard.as_ref() {
            if self.fresh(stored, &sources) {
                return Ok(stored.value.clone());
            }
        }
        match compute().await {
            Ok((value, true)) => {
                let at = Instant::now();
                let weight = (self.weigh)(&value) + OVERHEAD;
                *guard = Some(Stored {
                    value: value.clone(),
                    sources,
                    at,
                });
                drop(guard);
                self.keep(&key, weight);
                Ok(value)
            }
            Ok((value, false)) => {
                *guard = None;
                drop(guard);
                self.forget(&key, &slot);
                Ok(value)
            }
            Err(e) => {
                *guard = None;
                drop(guard);
                self.forget(&key, &slot);
                Err(e)
            }
        }
    }

    #[cfg(test)]
    fn weight(&self) -> usize {
        self.table.lock().unwrap().weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn memo(budget: usize) -> Memo<&'static str, usize> {
        Memo::new(budget, Duration::from_secs(60), Duration::from_secs(10), |_| 0)
    }

    fn hosted(stamp: &str) -> Sources {
        Sources {
            stamps: vec![stamp.to_string()],
            upstream: false,
        }
    }

    async fn ask(m: &Memo<&'static str, usize>, key: &'static str, s: Sources, n: &AtomicUsize) -> usize {
        m.get_or_compute::<(), _, _>(key, s, || async {
            Ok((n.fetch_add(1, Ordering::SeqCst) + 1, true))
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_hosted_answer_lives_until_its_stamp_moves() {
        let (m, n) = (memo(1 << 20), AtomicUsize::new(0));
        assert_eq!(ask(&m, "k", hosted("a"), &n).await, 1);
        assert_eq!(ask(&m, "k", hosted("a"), &n).await, 1);
        assert_eq!(ask(&m, "k", hosted("b"), &n).await, 2, "the stamp moved");
        assert_eq!(ask(&m, "k", hosted("b"), &n).await, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_upstream_answer_lives_until_the_age_cap_only() {
        let (m, n) = (memo(1 << 20), AtomicUsize::new(0));
        let up = Sources {
            stamps: Vec::new(),
            upstream: true,
        };
        assert_eq!(ask(&m, "k", up.clone(), &n).await, 1);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(ask(&m, "k", up.clone(), &n).await, 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(ask(&m, "k", up, &n).await, 2, "past the cap");
    }

    #[tokio::test]
    async fn keys_never_share_an_entry() {
        let (m, n) = (memo(1 << 20), AtomicUsize::new(0));
        assert_eq!(ask(&m, "view-a", hosted("s"), &n).await, 1);
        assert_eq!(ask(&m, "view-b", hosted("s"), &n).await, 2);
    }

    #[tokio::test]
    async fn concurrent_callers_compute_once() {
        let m = Arc::new(memo(1 << 20));
        let n = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (m, n, gate) = (m.clone(), n.clone(), gate.clone());
            tasks.push(tokio::spawn(async move {
                m.get_or_compute::<(), _, _>("k", hosted("a"), || async {
                    gate.notified().await;
                    Ok((n.fetch_add(1, Ordering::SeqCst) + 1, true))
                })
                .await
                .unwrap()
            }));
        }
        tokio::task::yield_now().await;
        gate.notify_one();
        for t in tasks {
            assert_eq!(t.await.unwrap(), 1);
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_waits_a_bounded_time_then_computes_alone() {
        let m = Arc::new(memo(1 << 20));
        let held = m.clone();
        let slow = tokio::spawn(async move {
            held.get_or_compute::<(), _, _>("k", hosted("a"), || async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok((1, true))
            })
            .await
        });
        tokio::task::yield_now().await;
        let alone = m
            .get_or_compute::<(), _, _>("k", hosted("a"), || async { Ok((2, true)) })
            .await
            .unwrap();
        assert_eq!(alone, 2);
        slow.abort();
    }

    #[tokio::test]
    async fn a_partial_answer_or_an_error_is_never_kept() {
        let m = memo(1 << 20);
        let v = m
            .get_or_compute::<(), _, _>("k", hosted("a"), || async { Ok((1, false)) })
            .await
            .unwrap();
        assert_eq!(v, 1);
        let err = m
            .get_or_compute::<&str, _, _>("k", hosted("a"), || async { Err("down") })
            .await;
        assert_eq!(err, Err("down"));
        let n = AtomicUsize::new(10);
        assert_eq!(ask(&m, "k", hosted("a"), &n).await, 11, "nothing was kept");
        assert_eq!(m.table.lock().unwrap().slots.len(), 1);
    }

    #[tokio::test]
    async fn the_budget_evicts_the_oldest() {
        let m = memo(3 * OVERHEAD);
        let n = AtomicUsize::new(0);
        for key in ["a", "b", "c", "d"] {
            ask(&m, key, hosted("s"), &n).await;
        }
        assert!(m.weight() <= 3 * OVERHEAD);
        assert_eq!(ask(&m, "a", hosted("s"), &n).await, 5, "the oldest went first");
        assert_eq!(ask(&m, "d", hosted("s"), &n).await, 4);
    }
}
