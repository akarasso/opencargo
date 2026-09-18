use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, OwnedSemaphorePermit};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tracing::error;

use crate::domain::{Audience, DomainEvent, Format, ResolutionCounts, RuleVerdict, Verdict};
use crate::ports::events::Events;
use crate::ports::policy::NewResolution;

use super::rules::osv_severity;
use super::{facts, rules, Pending, Resolution, Shared, BATCH};

type Done = (Resolution, Vec<Option<RuleVerdict>>);

#[derive(Default)]
struct Counts {
    count: u64,
    would_block: u64,
    unknown: u64,
}

/// One coalesced `policy.resolution` per `(requested_repo, member_repo)`
/// pair per flush, at most one send per `notify_period`.
#[derive(Default)]
pub(crate) struct Notify {
    last: Option<Instant>,
    pending: HashMap<(String, String), Counts>,
}

impl Notify {
    pub fn pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn offer(
        &mut self,
        rows: &[(Resolution, Vec<RuleVerdict>)],
        events: &dyn Events,
        period: Duration,
    ) {
        for (r, verdicts) in rows {
            let pair = (r.requested_repo.clone(), r.member_repo.clone());
            let counts = self.pending.entry(pair).or_default();
            counts.count += 1;
            if verdicts.iter().any(|v| v.verdict == Verdict::WouldBlock) {
                counts.would_block += 1;
            } else if verdicts.iter().any(|v| v.verdict == Verdict::Unknown) {
                counts.unknown += 1;
            }
        }
        self.send_due(events, period);
    }

    pub fn send_due(&mut self, events: &dyn Events, period: Duration) {
        if self.pending.is_empty() || self.last.is_some_and(|at| at.elapsed() < period) {
            return;
        }
        for ((repo, member), counts) in self.pending.drain() {
            events.emit(
                DomainEvent::PolicyResolution(ResolutionCounts {
                    repo,
                    member,
                    count: counts.count,
                    would_block: counts.would_block,
                    unknown: counts.unknown,
                }),
                Audience::Admin,
            );
        }
        self.last = Some(Instant::now());
    }
}

/// A `select!` over the channel, the gather tasks, the next permit for
/// an event waiting on a full `inflight`, a tick that runs only while
/// there is timed work, and the spawned flushes: an idle writer holds no
/// timer and is woken by the channel alone; waking restarts the tick so
/// the first flush after idleness has a full period to fill. While an
/// event waits for a slot the channel buffers, but the tick, the joins
/// and the flushes are still served: a slow upstream stalls one slot,
/// never the loop.
pub(crate) async fn run_writer(mut rx: mpsc::Receiver<Pending>, shared: Arc<Shared>) {
    let mut tasks: JoinSet<Done> = JoinSet::new();
    let mut flushes: JoinSet<()> = JoinSet::new();
    let mut ready: Vec<Done> = Vec::new();
    let mut waiting: VecDeque<Pending> = VecDeque::new();
    let mut tick = tokio::time::interval(shared.tuning.flush_period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut was_awake = false;
    loop {
        let awake = !ready.is_empty()
            || shared.holds_oci_state()
            || shared.notify.lock().unwrap().pending();
        if awake && !was_awake {
            tick.reset();
        }
        was_awake = awake;
        tokio::select! {
            received = rx.recv(), if waiting.is_empty() => match received {
                Some(p) => {
                    if let Some(p) = classify(&shared, p).await {
                        admit(&shared, p, &mut tasks, &mut waiting);
                    }
                }
                None => break,
            },
            permit = shared.inflight.clone().acquire_owned(), if !waiting.is_empty() => {
                if let (Ok(permit), Some(p)) = (permit, waiting.pop_front()) {
                    spawn_work(&shared, p, permit, &mut tasks);
                }
            }
            Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                if let Ok(done) = done {
                    ready.push(done);
                    if ready.len() >= BATCH {
                        flush(&shared, &mut ready, &mut flushes);
                    }
                }
            }
            _ = tick.tick(), if awake => {
                flush(&shared, &mut ready, &mut flushes);
                for p in facts::release_parked(&shared) {
                    admit(&shared, p, &mut tasks, &mut waiting);
                }
                facts::sweep_children(&shared);
                shared.notify.lock().unwrap().send_due(shared.events.as_ref(), shared.tuning.notify_period);
            }
            Some(_) = flushes.join_next(), if !flushes.is_empty() => {}
        }
    }
    flush(&shared, &mut ready, &mut flushes);
    while flushes.join_next().await.is_some() {}
}

/// An OCI event is classified inline, in arrival order; `None` is parked
/// or suppressed.
async fn classify(shared: &Arc<Shared>, p: Pending) -> Option<Pending> {
    if p.format == Format::Oci {
        facts::oci_classify(shared, p).await
    } else {
        Some(p)
    }
}

/// Spawns at once when a slot is free, else queues for the next permit.
fn admit(
    shared: &Arc<Shared>,
    p: Pending,
    tasks: &mut JoinSet<Done>,
    waiting: &mut VecDeque<Pending>,
) {
    match shared.inflight.clone().try_acquire_owned() {
        Ok(permit) => spawn_work(shared, p, permit, tasks),
        Err(_) => waiting.push_back(p),
    }
}

fn spawn_work(
    shared: &Arc<Shared>,
    p: Pending,
    permit: OwnedSemaphorePermit,
    tasks: &mut JoinSet<Done>,
) {
    let shared = shared.clone();
    tasks.spawn(async move {
        let _permit = permit;
        let cfg = shared.config_for(&p.member.name);
        let resolution = facts::gather(&shared, &cfg, p).await;
        let verdicts = rules::evaluate_all(&shared.rules, &cfg, &resolution, Utc::now());
        (resolution, verdicts)
    });
}

/// Spawned: the receive loop never waits on OSV or SQLite.
fn flush(shared: &Arc<Shared>, ready: &mut Vec<Done>, flushes: &mut JoinSet<()>) {
    if ready.is_empty() {
        return;
    }
    let mut batch = std::mem::take(ready);
    let shared = shared.clone();
    flushes.spawn(async move {
        osv_severity::evaluate_batch(
            shared.scanner.as_ref(),
            &shared.osv_memo,
            |r| shared.config_for(&r.member_repo).osv_severity,
            &mut batch,
        )
        .await;
        let rows: Vec<(Resolution, Vec<RuleVerdict>)> = batch
            .into_iter()
            .map(|(r, verdicts)| (r, verdicts.into_iter().flatten().collect()))
            .collect();
        let commands: Vec<NewResolution<'_>> = rows.iter().map(command).collect();
        match shared.store.insert_batch(&commands, Utc::now()).await {
            Ok(_) => shared.notify.lock().unwrap().offer(
                &rows,
                shared.events.as_ref(),
                shared.tuning.notify_period,
            ),
            Err(e) => {
                error!(error = %e, rows = rows.len(), "policy batch insert failed; rows dropped")
            }
        }
    });
}

/// One gathered resolution as the store takes it: borrowed, so a batch of
/// sixty-four costs no second copy of what the rules already produced.
fn command<'a>((r, verdicts): &'a (Resolution, Vec<RuleVerdict>)) -> NewResolution<'a> {
    NewResolution {
        requested_repo: &r.requested_repo,
        member_repo: &r.member_repo,
        format: r.format.as_str(),
        name: &r.name,
        version: r.version.as_deref(),
        digest: r.digest.as_deref(),
        published_at: r.published_at,
        date_source: r.facts.date_source,
        actor: &r.actor.name,
        actor_kind: r.actor.kind.as_str(),
        user_id: r.actor.user_id,
        verdicts,
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
