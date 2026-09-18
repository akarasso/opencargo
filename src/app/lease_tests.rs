use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;

/// The lease table in memory, with the adapter's semantics.
#[derive(Default)]
struct Table {
    row: Mutex<Option<LeaseRow>>,
    failing: AtomicBool,
    renewals: AtomicUsize,
}

#[async_trait]
impl LeaseStore for Table {
    async fn ensure(&self) -> Result<(), StoreError> {
        Ok(())
    }

    async fn acquire(
        &self,
        name: &str,
        owner: &str,
        version: &str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> Result<Acquired, StoreError> {
        let mut row = self.row.lock().unwrap();
        let free = match row.as_ref() {
            None => true,
            Some(r) => r.owner == owner || r.renewed_at < now - chrono::Duration::from_std(stale_after).unwrap(),
        };
        if !free {
            return Ok(Acquired::HeldBy(row.clone()));
        }
        let taken = LeaseRow {
            name: name.to_string(),
            owner: owner.to_string(),
            version: version.to_string(),
            acquired_at: now,
            renewed_at: now,
        };
        *row = Some(taken.clone());
        Ok(Acquired::Taken(taken))
    }

    async fn renew(&self, _: &str, owner: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        let mut row = self.row.lock().unwrap();
        match row.as_mut() {
            Some(r) if r.owner == owner => {
                r.renewed_at = now;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release(&self, _: &str, owner: &str) -> Result<(), StoreError> {
        let mut row = self.row.lock().unwrap();
        if row.as_ref().is_some_and(|r| r.owner == owner) {
            *row = None;
        }
        Ok(())
    }

    async fn current(&self, _: &str) -> Result<Option<LeaseRow>, StoreError> {
        Ok(self.row.lock().unwrap().clone())
    }
}

/// Wall time that moves with tokio's paused clock.
struct Paused {
    base: DateTime<Utc>,
    start: Instant,
}

impl Clock for Paused {
    fn now(&self) -> DateTime<Utc> {
        self.base + chrono::Duration::from_std(self.start.elapsed()).unwrap()
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(Paused {
        base: Utc::now(),
        start: Instant::now(),
    })
}

const TERMS: LeaseTerms = LeaseTerms {
    wait: Duration::from_secs(60),
    stale_after: Duration::from_secs(30),
    renew: Duration::from_secs(10),
};

fn table() -> Arc<Table> {
    Arc::new(Table::default())
}

#[tokio::test(start_paused = true)]
async fn take_waits_then_succeeds_when_the_holder_releases() {
    let store = table();
    let clock = clock();
    let first = LeaseGuard::take(store.clone(), clock.clone(), "a", "1", TERMS).await.unwrap();
    let started = Instant::now();
    let second = tokio::spawn({
        let (store, clock) = (store.clone(), clock.clone());
        async move { LeaseGuard::take(store, clock, "b", "1", TERMS).await }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    first.release().await;
    let second = second.await.unwrap().unwrap();
    assert_eq!(second.handle().owner(), "b");
    assert!(started.elapsed() < TERMS.stale_after, "taken on release, not on staleness");
}

#[tokio::test(start_paused = true)]
async fn take_fails_with_the_holder_identity() {
    let store = table();
    let clock = clock();
    let _held = LeaseGuard::take(store.clone(), clock.clone(), "holder-1", "0.9.1", TERMS).await.unwrap();
    let started = Instant::now();
    let err = LeaseGuard::take(store, clock, "b", "1", TERMS).await.err().unwrap();
    assert!(started.elapsed() >= TERMS.wait);
    let text = err.to_string();
    assert!(text.contains("holder-1") && text.contains("0.9.1") && text.contains("renewed at"), "{text}");
    assert!(!text.contains('/'), "no path in {text}");
}

#[tokio::test(start_paused = true)]
async fn a_restart_outlives_its_own_stale_lease() {
    let store = table();
    let clock = clock();
    let dead = LeaseGuard::take(store.clone(), clock.clone(), "dead", "1", TERMS).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(dead);
    let started = Instant::now();
    let fresh = LeaseGuard::take(store, clock, "fresh", "1", TERMS).await.unwrap();
    assert_eq!(fresh.handle().owner(), "fresh");
    assert!(started.elapsed() <= TERMS.stale_after + LeaseTerms::POLL);
}

#[tokio::test(start_paused = true)]
async fn lost_renewal_clears_held_once_and_reacquires() {
    let store = table();
    let clock = clock();
    let guard = LeaseGuard::take(store.clone(), clock.clone(), "a", "1", TERMS).await.unwrap();
    let handle = guard.handle();
    *store.row.lock().unwrap() = Some(LeaseRow {
        name: WRITER.to_string(),
        owner: "intruder".to_string(),
        version: "1".to_string(),
        acquired_at: clock.now(),
        renewed_at: clock.now(),
    });
    tokio::time::sleep(TERMS.renew + Duration::from_secs(1)).await;
    assert_eq!(handle.status(), LeaseStatus::Lost);
    tokio::time::sleep(TERMS.stale_after + TERMS.renew).await;
    assert_eq!(handle.status(), LeaseStatus::Held, "taken back once the intruder went stale");
    assert_eq!(store.row.lock().unwrap().as_ref().unwrap().owner, "a");
}

#[tokio::test(start_paused = true)]
async fn renew_error_does_not_clear_held() {
    let store = table();
    let clock = clock();
    let guard = LeaseGuard::take(store.clone(), clock, "a", "1", TERMS).await.unwrap();
    store.failing.store(true, Ordering::SeqCst);
    tokio::time::sleep(TERMS.renew * 3 + Duration::from_secs(1)).await;
    assert!(store.renewals.load(Ordering::SeqCst) >= 3);
    assert!(guard.handle().held());
}

#[tokio::test(start_paused = true)]
async fn release_stops_the_renewals() {
    let store = table();
    let guard = LeaseGuard::take(store.clone(), clock(), "a", "1", TERMS).await.unwrap();
    guard.release().await;
    let before = store.renewals.load(Ordering::SeqCst);
    tokio::time::sleep(TERMS.renew * 3).await;
    assert_eq!(store.renewals.load(Ordering::SeqCst), before);
    assert!(store.row.lock().unwrap().is_none());
}

#[test]
fn a_disabled_lease_behaves_as_the_holder() {
    let handle = LeaseHandle::disabled();
    assert!(handle.held());
    assert_eq!(handle.status(), LeaseStatus::Disabled);
}
