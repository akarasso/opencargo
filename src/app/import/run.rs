//! The run: probe, preflight, discover into the journal, copy on two lanes,
//! seal each package, report.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use super::plan::{PlanRules, Planner, RulesOf};
use super::report::{self, tagged, Report};
use crate::domain::import::{GapKind, ItemStatus, EXIT_ABORTED, EXIT_NOT_STARTED};
use crate::domain::Format;
use crate::ports::clock::Clock;
use crate::ports::import::{
    Batch, CopyError, Gap, ImportJournal, JournalError, Journaled, Lane, Outcome, Presence, Probe,
    RunHeader, Sink, Source, SourceError, SourceFilter, TargetAdmin, TargetError, TargetIdentity,
};

pub struct RunOpts {
    pub plan: PlanRules,
    pub filter: SourceFilter,
    pub concurrency: usize,
    pub oci_concurrency: usize,
    pub dry_run: bool,
    pub fail_fast: bool,
    pub allow_incomplete: bool,
    pub create_repos: bool,
    pub no_retag: bool,
    pub retries: u32,
    pub report_collapse: usize,
    /// A new run re-walks every stream; a resume only the unfinished ones.
    pub fresh: bool,
    pub owner: String,
    pub header: RunHeader,
    pub heartbeat: Duration,
}

pub struct RunImport<'a> {
    pub journal: Arc<dyn ImportJournal>,
    pub source: Arc<dyn Source>,
    pub sinks: HashMap<Format, Arc<dyn Sink>>,
    pub admin: Arc<dyn TargetAdmin>,
    pub clock: Arc<dyn Clock>,
    pub names: RulesOf<'a>,
}

#[derive(Debug)]
pub struct Finished {
    pub code: u8,
    pub report: Option<Report>,
    /// Why the run stopped early, when it did.
    pub message: Option<String>,
    pub probe: Option<Probe>,
    /// Plan lines worth printing: repositories that would be created.
    pub notes: Vec<String>,
}

impl Finished {
    fn refused(message: impl Into<String>) -> Self {
        Self { code: EXIT_NOT_STARTED, report: None, message: Some(message.into()), probe: None, notes: Vec::new() }
    }
}

fn journal_err(e: JournalError) -> String {
    format!("state file: {e}")
}

/// Asserts write, derived read, hosted and format on every target
/// repository, off the one row the target's own permission view gives.
pub fn assert_targets(
    id: &TargetIdentity,
    wanted: &[(String, Option<Format>)],
    deferred: &HashSet<String>,
) -> Result<(), String> {
    if id.username == "anonymous" {
        return Err("the target token is not valid: the target answered as anonymous".into());
    }
    let mut formats: BTreeMap<&str, HashSet<Format>> = BTreeMap::new();
    for (repo, f) in wanted {
        if let Some(f) = f {
            formats.entry(repo).or_default().insert(*f);
        }
    }
    for (repo, fs) in &formats {
        if fs.len() > 1 {
            let mut names: Vec<&str> = fs.iter().map(|f| f.as_str()).collect();
            names.sort();
            return Err(format!("repository {repo} would receive several formats ({}); route them apart with --map", names.join(", ")));
        }
    }
    for (repo, format) in wanted {
        let Some(row) = id.permissions.iter().find(|p| &p.repository == repo) else {
            if deferred.contains(repo) {
                continue;
            }
            return Err(format!(
                "repository {repo} is not visible to this token: it may not exist, or the token lacks read on a private repository — run --create-repos or grant read"
            ));
        };
        if row.repo_type != "hosted" {
            return Err(format!("repository {repo} is a {} repository; an import writes into hosted ones", row.repo_type));
        }
        if let Some(f) = format {
            if row.format != f.as_str() {
                return Err(format!("repository {repo} is a {} repository, the plan routes {} items to it", row.format, f.as_str()));
            }
        }
        if !row.can_write {
            return Err(format!("the target token is missing write on {repo} (source: {})", row.source));
        }
        if !(row.can_read || row.visibility == "public") {
            return Err(format!("the target token is missing read on {repo} (source: {})", row.source));
        }
    }
    Ok(())
}

enum Step {
    Done(Outcome),
    Abort(String),
}

async fn heartbeat_loop(journal: Arc<dyn ImportJournal>, clock: Arc<dyn Clock>, owner: String, every: Duration) -> JournalError {
    loop {
        tokio::time::sleep(every).await;
        match journal.heartbeat(&owner, clock.now()).await {
            Err(e @ JournalError::Lost(_)) => return e,
            Err(e) => warn!("heartbeat: {e}"),
            Ok(()) => {}
        }
    }
}

fn failed(kind: GapKind, msg: &str) -> Outcome {
    Outcome { status: ItemStatus::Failed, bytes: None, sha256: None, error: Some(tagged(kind, msg)), note: None, gaps: Vec::new() }
}

fn backoff(attempt: u32) -> Duration {
    use rand::Rng;
    let base = 100u64 << attempt.min(6);
    Duration::from_millis(base + rand::thread_rng().gen_range(0..=base))
}

async fn process(sink: &dyn Sink, it: &Journaled, retries: u32, no_retag: bool) -> Step {
    let p = &it.planned;
    let mut attempt = 0;
    loop {
        let result = match sink.present(p).await {
            Ok(Presence::Same) => {
                return Step::Done(Outcome { status: ItemStatus::Skipped, bytes: None, sha256: None, error: None, note: None, gaps: Vec::new() })
            }
            Ok(Presence::Present) => {
                return Step::Done(Outcome {
                    status: ItemStatus::Skipped,
                    bytes: None,
                    sha256: None,
                    error: None,
                    note: Some(tagged(GapKind::SkippedUnverifiable, "the target has it and its protocol exposes no checksum to compare")),
                    gaps: Vec::new(),
                })
            }
            Ok(Presence::Different(d)) if p.target_format != Format::Oci || no_retag => {
                return Step::Done(failed(GapKind::Failed, &format!("conflict: the target holds different content ({d}) at this immutable coordinate")))
            }
            Ok(_) => sink.copy(p).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(c) => {
                return Step::Done(Outcome {
                    status: ItemStatus::Copied,
                    bytes: Some(c.bytes),
                    sha256: Some(c.sha256),
                    error: None,
                    note: c.note,
                    gaps: c.gaps,
                })
            }
            Err(CopyError::Conflict(_)) => {
                return Step::Done(Outcome { status: ItemStatus::Skipped, bytes: None, sha256: None, error: None, note: None, gaps: Vec::new() })
            }
            Err(CopyError::Transient(m)) if attempt < retries => {
                attempt += 1;
                warn!(item = %p.item.source_ref, attempt, "transient failure, retrying: {m}");
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(CopyError::Transient(m)) | Err(CopyError::Permanent(m)) => return Step::Done(failed(GapKind::Failed, &m)),
            Err(e @ CopyError::TooLarge { .. }) => return Step::Done(failed(GapKind::TooLarge, &e.to_string())),
            Err(CopyError::Refused(m)) => return Step::Done(failed(GapKind::TargetRefused, &m)),
            Err(CopyError::Stalled(m)) => return Step::Abort(m),
        }
    }
}

impl RunImport<'_> {
    async fn discover(&self, opts: &RunOpts) -> Result<Vec<Gap>, String> {
        let sinks: HashSet<Format> = self.sinks.keys().copied().collect();
        let planner = Planner { rules: &opts.plan, sinks: &sinks, names: self.names };
        let streams = match self.source.streams(&opts.filter).await {
            Ok(s) => s,
            Err(e) => return Err(format!("listing the source: {e}")),
        };
        self.journal.streams(&streams).await.map_err(journal_err)?;
        let mut failures = Vec::new();
        for stream in &streams {
            let (mut at, done) = self.journal.cursor(stream).await.map_err(journal_err)?;
            if done {
                continue;
            }
            let mut restarted = at.is_none();
            loop {
                let mut batch = Batch::default();
                match self.source.discover(&opts.filter, stream, at.clone(), &mut batch).await {
                    Ok((next, done)) => {
                        let (planned, mut gaps) = planner.plan(batch.items);
                        gaps.extend(batch.gaps);
                        info!(stream = %stream, items = planned.len(), gaps = gaps.len(), "discovered");
                        self.journal
                            .record(stream, restarted, &planned, &gaps, &next, done)
                            .await
                            .map_err(journal_err)?;
                        restarted = false;
                        at = next;
                        if done {
                            break;
                        }
                    }
                    Err(SourceError::Auth(m)) => return Err(format!("the source refused the credentials: {m}")),
                    Err(e) => {
                        warn!(stream = %stream, "listing failed: {e}");
                        failures.push(Gap::new(GapKind::ListingIncomplete, stream, format!("listing stopped: {e}")));
                        break;
                    }
                }
            }
        }
        Ok(failures)
    }

    async fn preflight(&self, opts: &RunOpts, notes: &mut Vec<String>) -> Result<(), String> {
        let id = match self.admin.identity().await {
            Ok(id) => id,
            Err(TargetError::Unsupported(what)) => {
                warn!("the target does not answer {what}: rights are proved per item instead");
                return Ok(());
            }
            Err(e) => return Err(format!("target preflight: {e}")),
        };
        let wanted: Vec<(String, Option<Format>)> =
            self.journal.targets().await.map_err(journal_err)?.into_iter().map(|(r, f)| (r, Some(f))).collect();
        let missing: Vec<(String, Format)> = wanted
            .iter()
            .filter(|(r, _)| !id.permissions.iter().any(|p| &p.repository == r))
            .filter_map(|(r, f)| f.map(|f| (r.clone(), f)))
            .collect();
        if !opts.create_repos || missing.is_empty() {
            return assert_targets(&id, &wanted, &HashSet::new());
        }
        let deferred: HashSet<String> = missing.iter().map(|(r, _)| r.clone()).collect();
        assert_targets(&id, &wanted, &deferred)?;
        if opts.dry_run {
            for (r, f) in &missing {
                notes.push(format!("would-create {r} ({}) — write right unverifiable until created", f.as_str()));
            }
            return Ok(());
        }
        let grant = id.role != "admin";
        if grant && !self.admin.user_exists(&id.username).await.map_err(|e| e.to_string())? {
            return Err(format!(
                "--create-repos cannot grant rights to '{}', which has no user row; use a DB user's token",
                id.username
            ));
        }
        for (r, f) in &missing {
            self.admin.create_repository(r, *f).await.map_err(|e| e.to_string())?;
            if grant {
                self.admin.grant(&id.username, r, true, true).await.map_err(|e| e.to_string())?;
            }
            notes.push(format!("created {r} ({})", f.as_str()));
        }
        let id = self.admin.identity().await.map_err(|e| format!("target preflight: {e}"))?;
        assert_targets(&id, &wanted, &HashSet::new())
    }

    async fn copy(&self, opts: &RunOpts) -> Result<Option<String>, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let abort: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let mut set = tokio::task::JoinSet::new();
        for (lane, n) in [(Lane::File, opts.concurrency.max(1)), (Lane::Blob, opts.oci_concurrency.max(1))] {
            for _ in 0..n {
                let (journal, sinks, clock) = (self.journal.clone(), self.sinks.clone(), self.clock.clone());
                let (stop, abort) = (stop.clone(), abort.clone());
                let (retries, no_retag, fail_fast) = (opts.retries, opts.no_retag, opts.fail_fast);
                set.spawn(async move {
                    while !stop.load(Ordering::SeqCst) {
                        let Some(it) = journal.claim(lane, clock.now()).await.map_err(journal_err)? else { break };
                        let Some(sink) = sinks.get(&it.planned.target_format) else {
                            let o = failed(GapKind::UnsupportedFormat, "no sink");
                            journal.complete(&it.planned.item.source_ref, &o).await.map_err(journal_err)?;
                            continue;
                        };
                        let started = std::time::Instant::now();
                        match process(sink.as_ref(), &it, retries, no_retag).await {
                            Step::Done(o) => {
                                let p = &it.planned;
                                info!(
                                    status = o.status.as_str(),
                                    item = %format!("{}/{}@{}", p.target_repo, p.target_name, p.item.coord.version),
                                    bytes = o.bytes.unwrap_or(0),
                                    ms = started.elapsed().as_millis() as u64,
                                    error = o.error.as_deref().unwrap_or(""),
                                    "item"
                                );
                                let is_failure = o.status == ItemStatus::Failed;
                                journal.complete(&p.item.source_ref, &o).await.map_err(journal_err)?;
                                if is_failure && fail_fast {
                                    *abort.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(format!("--fail-fast: {} failed", p.item.source_ref));
                                    stop.store(true, Ordering::SeqCst);
                                }
                            }
                            Step::Abort(m) => {
                                let requeue = Outcome {
                                    status: ItemStatus::Pending,
                                    bytes: None,
                                    sha256: None,
                                    error: None,
                                    note: None,
                                    gaps: Vec::new(),
                                };
                                journal.complete(&it.planned.item.source_ref, &requeue).await.map_err(journal_err)?;
                                *abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(m);
                                stop.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    Ok::<(), String>(())
                });
            }
        }
        let mut first_err = None;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => first_err = first_err.or(Some(e)),
                Err(e) => first_err = first_err.or(Some(format!("worker: {e}"))),
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        let reason = abort.lock().unwrap_or_else(|e| e.into_inner()).take();
        Ok(reason)
    }

    async fn seal(&self) -> Result<Vec<Gap>, String> {
        let mut failures = Vec::new();
        for u in self.journal.unsealed().await.map_err(journal_err)? {
            let Some(sink) = self.sinks.get(&u.format) else { continue };
            match sink.seal(&u.target_repo, &u.name, &u.extra, &u.landed).await {
                Ok(gaps) => self.journal.sealed(&u.target_repo, &u.name, &gaps).await.map_err(journal_err)?,
                Err(e) => failures.push(Gap::new(
                    GapKind::Failed,
                    format!("{}/{}", u.target_repo, u.name),
                    format!("sealing the package: {e}"),
                )),
            }
        }
        Ok(failures)
    }

    pub async fn run(&self, opts: &RunOpts, cancel: impl Future<Output = ()>) -> Finished {
        let probe = match self.source.probe().await {
            Ok(p) => p,
            Err(e) => return Finished::refused(format!("source probe: {e}")),
        };
        info!(product = %probe.product, version = probe.version.as_deref().unwrap_or("?"), "source probed");
        if let Err(e) = self.journal.begin(&opts.header, &opts.owner, opts.fresh, self.clock.now()).await {
            return Finished::refused(journal_err(e));
        }
        let mut beat = tokio::spawn(heartbeat_loop(
            self.journal.clone(),
            self.clock.clone(),
            opts.owner.clone(),
            opts.heartbeat,
        ));
        let mut finished = tokio::select! {
            f = self.run_claimed(opts, cancel) => f,
            lost = &mut beat => {
                let why = lost.map_or_else(|e| format!("heartbeat: {e}"), journal_err);
                Finished { code: EXIT_ABORTED, report: None, message: Some(why), probe: None, notes: Vec::new() }
            }
        };
        beat.abort();
        let phase = match finished.code {
            EXIT_NOT_STARTED => "refused",
            EXIT_ABORTED => "aborted",
            _ if opts.dry_run => "planned",
            _ => "finished",
        };
        if let Err(e) = self.journal.finish(phase, self.clock.now()).await {
            warn!("state file: {e}");
        }
        if let Some(r) = finished.report.as_mut() {
            r.phase = Some(phase.to_string());
            r.exit_code = finished.code;
        }
        finished.probe = Some(probe);
        finished
    }

    async fn run_claimed(&self, opts: &RunOpts, cancel: impl Future<Output = ()>) -> Finished {
        let mut notes = Vec::new();
        let mut run_gaps = match self.discover(opts).await {
            Ok(g) => g,
            Err(e) => return Finished::refused(e),
        };
        if let Err(e) = self.journal.take_collisions().await {
            return Finished::refused(journal_err(e));
        }
        if let Err(e) = self.preflight(opts, &mut notes).await {
            let mut f = Finished::refused(e);
            f.notes = notes;
            f.report = report::from_journal(self.journal.as_ref(), opts.allow_incomplete, opts.report_collapse).await.ok();
            return f;
        }
        let mut aborted = None;
        if !opts.dry_run {
            tokio::select! {
                r = self.copy(opts) => match r {
                    Ok(reason) => aborted = reason,
                    Err(e) => aborted = Some(e),
                },
                _ = cancel => aborted = Some("interrupted".to_string()),
            }
            if aborted.is_none() {
                match self.seal().await {
                    Ok(g) => run_gaps.extend(g),
                    Err(e) => aborted = Some(e),
                }
            }
        }
        if let Err(e) = self.journal.replace_run_gaps(&run_gaps).await {
            aborted = aborted.or(Some(journal_err(e)));
        }
        let report = report::from_journal(self.journal.as_ref(), opts.allow_incomplete, opts.report_collapse).await;
        let (code, report) = match report {
            Ok(r) => (if aborted.is_some() { EXIT_ABORTED } else { r.exit_code }, Some(r)),
            Err(e) => (EXIT_ABORTED, { aborted = aborted.or(Some(journal_err(e))); None }),
        };
        Finished { code, report, message: aborted, probe: None, notes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::import::TargetRepo;

    fn row(repo: &str, kind: &str, format: &str, vis: &str, read: bool, write: bool) -> TargetRepo {
        TargetRepo {
            repository: repo.into(),
            repo_type: kind.into(),
            format: format.into(),
            visibility: vis.into(),
            can_read: read,
            can_write: write,
            source: "grant".into(),
        }
    }

    fn id(user: &str, rows: Vec<TargetRepo>) -> TargetIdentity {
        TargetIdentity { username: user.into(), role: "reader".into(), permissions: rows }
    }

    #[test]
    fn preflight_asserts_write_derived_read_hosted_and_format() {
        let want = |r: &str| vec![(r.to_string(), Some(Format::Npm))];
        let none = HashSet::new();
        let ok = id("u", vec![row("t", "hosted", "npm", "private", true, true)]);
        assert!(assert_targets(&ok, &want("t"), &none).is_ok());
        let err = assert_targets(&id("anonymous", vec![]), &want("t"), &none).unwrap_err();
        assert!(err.contains("anonymous"), "{err}");
        let ro = id("u", vec![row("t", "hosted", "npm", "private", true, false)]);
        let err = assert_targets(&ro, &want("t"), &none).unwrap_err();
        assert!(err.contains("write on t") && err.contains("grant"), "{err}");
        let public_wo = id("u", vec![row("t", "hosted", "npm", "public", false, true)]);
        assert!(assert_targets(&public_wo, &want("t"), &none).is_ok());
        let err = assert_targets(&id("u", vec![]), &want("t"), &none).unwrap_err();
        assert!(err.contains("not visible to this token"), "{err}");
        assert!(assert_targets(&id("u", vec![]), &want("t"), &["t".to_string()].into()).is_ok());
        let proxy = id("u", vec![row("t", "proxy", "npm", "private", true, true)]);
        assert!(assert_targets(&proxy, &want("t"), &none).unwrap_err().contains("proxy"));
        let cargo = id("u", vec![row("t", "hosted", "cargo", "private", true, true)]);
        assert!(assert_targets(&cargo, &want("t"), &none).unwrap_err().contains("cargo repository"));
        let two = vec![("t".to_string(), Some(Format::Npm)), ("t".to_string(), Some(Format::Cargo))];
        assert!(assert_targets(&ok, &two, &none).unwrap_err().contains("several formats"));
    }
}
