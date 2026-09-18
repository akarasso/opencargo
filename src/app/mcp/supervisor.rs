//! One task that keeps a sync child per `mcp` mirror, reconciled from the
//! repositories themselves: a mirror created at runtime gets a child, a
//! deleted one loses it before its next write, a retargeted one restarts.
//! Spawned by the binary, never by the state a test builds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::probe::{ProbeMirror, ProbeSettings};
use super::sync::{SyncError, SyncMirror, SyncReport};
use crate::domain::{DomainEvent, Format, RepoKind};
use crate::error::StoreError;
use crate::ports::events::{Events, Received};
use crate::ports::mcp_feed::FeedError;
use crate::ports::repositories::RepositoryStore;

const TICK: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(6 * 3600);

struct Child {
    upstream: String,
    notify: Arc<Notify>,
    handle: JoinHandle<()>,
}

/// The live children and one run lock per mirror, shared with the admin
/// route so an on-demand run never overlaps a scheduled one.
#[derive(Default)]
pub struct SyncHandles {
    children: Mutex<HashMap<i64, Child>>,
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
}

impl SyncHandles {
    pub fn lock_for(&self, repository: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|p| p.into_inner());
        locks.entry(repository).or_default().clone()
    }

    pub fn live(&self) -> Vec<i64> {
        let children = self.children.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<i64> = children.keys().copied().collect();
        ids.sort();
        ids
    }

    /// Wake a live child now; `false` when the mirror has none.
    pub fn wake(&self, repository: i64) -> bool {
        let children = self.children.lock().unwrap_or_else(|p| p.into_inner());
        children.get(&repository).map(|c| c.notify.notify_one()).is_some()
    }
}

/// One mirror's run under its lock.
pub async fn run_locked(
    handles: &SyncHandles,
    sync: &SyncMirror,
    repository: i64,
    name: &str,
    upstream: &str,
    full: bool,
) -> Result<SyncReport, SyncError> {
    let lock = handles.lock_for(repository);
    let _held = lock.lock().await;
    sync.run(repository, name, upstream, full).await
}

pub struct SyncSupervisor {
    handles: Arc<SyncHandles>,
    repos: Arc<dyn RepositoryStore>,
    sync: Arc<SyncMirror>,
    events: Arc<dyn Events>,
    intervals: Arc<HashMap<String, Duration>>,
    default_interval: Duration,
    probe: Option<Arc<ProbeMirror>>,
    probes: Arc<HashMap<String, Arc<ProbeSettings>>>,
}

impl SyncSupervisor {
    pub fn new(
        handles: Arc<SyncHandles>,
        repos: Arc<dyn RepositoryStore>,
        sync: Arc<SyncMirror>,
        events: Arc<dyn Events>,
        intervals: HashMap<String, Duration>,
    ) -> Self {
        Self {
            handles,
            repos,
            sync,
            events,
            intervals: Arc::new(intervals),
            default_interval: Duration::from_secs(3600),
            probe: None,
            probes: Arc::default(),
        }
    }

    /// Probe the remotes of the named mirrors after each of their runs.
    pub fn probing(mut self, probe: Arc<ProbeMirror>, settings: HashMap<String, Arc<ProbeSettings>>) -> Self {
        self.probe = Some(probe);
        self.probes = Arc::new(settings);
        self
    }

    /// The children the repositories call for, and no others.
    pub async fn reconcile(&self) -> Result<(), StoreError> {
        let wanted: HashMap<i64, (String, String)> = self
            .repos
            .all()
            .await?
            .into_iter()
            .filter(|r| r.fmt().ok() == Some(Format::Mcp) && r.kind().ok() == Some(RepoKind::Proxy))
            .filter_map(|r| r.upstream_url.clone().map(|u| (r.id, (r.name.clone(), u))))
            .collect();
        let mut children = self.handles.children.lock().unwrap_or_else(|p| p.into_inner());
        children.retain(|id, child| {
            let keep = wanted.get(id).is_some_and(|(_, up)| *up == child.upstream);
            if !keep {
                child.handle.abort();
            }
            keep
        });
        for (id, (name, upstream)) in wanted {
            if children.contains_key(&id) {
                continue;
            }
            let notify = Arc::new(Notify::new());
            let interval = self.intervals.get(&name).copied().unwrap_or(self.default_interval);
            let probe = self.probe.clone().zip(self.probes.get(&name).cloned());
            let handle = tokio::spawn(child(
                Mirror {
                    handles: self.handles.clone(),
                    sync: self.sync.clone(),
                    probe,
                    repository: id,
                    name,
                    upstream: upstream.clone(),
                    interval,
                },
                notify.clone(),
            ));
            children.insert(id, Child { upstream, notify, handle });
        }
        Ok(())
    }

    pub async fn run(self) {
        let mut events = self.events.subscribe();
        loop {
            if let Err(e) = self.reconcile().await {
                warn!(error = %e, "MCP sync supervisor could not read the repositories");
            }
            tokio::select! {
                received = events.recv() => match received {
                    Received::Event(e) if e.event != DomainEvent::RepositoriesChanged => continue,
                    Received::Closed => tokio::time::sleep(TICK).await,
                    _ => {}
                },
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

fn jittered(interval: Duration) -> Duration {
    let tenth = interval.as_secs_f64() / 10.0;
    let offset = rand::thread_rng().gen_range(-tenth..=tenth);
    Duration::from_secs_f64((interval.as_secs_f64() + offset).max(1.0))
}

/// After a failure the wait doubles up to six hours; a client error other
/// than 429 is ours to fix, and waits one interval.
fn wait_after(interval: Duration, failures: u32, error: Option<&SyncError>) -> Duration {
    if let Some(SyncError::Feed(FeedError::Status(s))) = error {
        if (400..500).contains(s) && *s != 429 {
            return jittered(interval);
        }
    }
    if failures == 0 {
        return jittered(interval);
    }
    interval.saturating_mul(1u32 << failures.min(10)).min(MAX_BACKOFF)
}

struct Mirror {
    handles: Arc<SyncHandles>,
    sync: Arc<SyncMirror>,
    probe: Option<(Arc<ProbeMirror>, Arc<ProbeSettings>)>,
    repository: i64,
    name: String,
    upstream: String,
    interval: Duration,
}

async fn child(m: Mirror, notify: Arc<Notify>) {
    let Mirror {
        handles,
        sync,
        probe,
        repository,
        name,
        upstream,
        interval,
    } = m;
    let mut failures = 0u32;
    loop {
        let outcome = run_locked(&handles, &sync, repository, &name, &upstream, false).await;
        let wait = match &outcome {
            Ok(report) => {
                failures = 0;
                info!(repository = %name, pages = report.pages, changed = report.changed, skipped = report.skipped, "MCP mirror synced");
                if let Some((probe, settings)) = &probe {
                    match probe.run(repository, &name, settings, false, None).await {
                        Ok(r) => info!(repository = %name, probed = r.probed, answered = r.answered, "MCP remotes probed"),
                        Err(e) => warn!(repository = %name, error = %e, "MCP probe run failed"),
                    }
                }
                wait_after(interval, 0, None)
            }
            Err(e) => {
                failures += 1;
                warn!(repository = %name, error = %e, failures, "MCP mirror sync failed; serving the last good rows");
                wait_after(interval, failures, Some(e))
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = notify.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_six_hours_and_a_client_error_waits_one_interval() {
        let hour = Duration::from_secs(3600);
        assert_eq!(wait_after(hour, 1, None), 2 * hour);
        assert_eq!(wait_after(hour, 2, None), 4 * hour);
        assert_eq!(wait_after(hour, 9, None), MAX_BACKOFF);
        let refused = SyncError::Feed(FeedError::Status(404));
        assert!(wait_after(hour, 5, Some(&refused)) <= hour + hour / 10);
        let busy = SyncError::Feed(FeedError::Status(429));
        assert_eq!(wait_after(hour, 3, Some(&busy)), MAX_BACKOFF.min(8 * hour));
        let first = wait_after(hour, 0, None);
        assert!(first >= hour - hour / 10 && first <= hour + hour / 10);
    }
}
