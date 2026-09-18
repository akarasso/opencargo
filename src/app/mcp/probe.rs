//! `ProbeMirror`: ask each remote of a mirror's latest versions for its
//! tools, one run per endpoint, the run and not the surface being the
//! freshness clock so a failing server backs off like a working one.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::error::StoreError;
use crate::ports::clock::Clock;
use crate::ports::events::Events;
use crate::ports::mcp::{McpStore, NewSurface, ProbeRun, ProbeRunRow};
use crate::ports::mcp_feed::{ProbeOptions, ToolProbe};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub url: String,
    pub sse: bool,
}

pub type Remotes = fn(&str) -> Vec<Remote>;
pub type SurfaceOf = fn(&str, &str, &[Value]) -> Result<NewSurface, String>;

pub const SSE_NOT_PROBED: &str = "sse transport not probed";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProbeReport {
    pub probed: usize,
    pub answered: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub struct ProbeSettings {
    pub options: ProbeOptions,
    pub interval: Duration,
    pub concurrency: usize,
}

pub struct ProbeMirror {
    mcp: Arc<dyn McpStore>,
    probe: Arc<dyn ToolProbe>,
    clock: Arc<dyn Clock>,
    events: Arc<dyn Events>,
    remotes: Remotes,
    surface: SurfaceOf,
}

/// The newest run of an endpoint, and how many failed in a row before it.
fn history<'a>(runs: &'a [ProbeRunRow], url: &str) -> (Option<&'a ProbeRunRow>, u32) {
    let mine: Vec<&ProbeRunRow> = runs.iter().filter(|r| r.remote_url == url).collect();
    let failures = mine.iter().take_while(|r| !r.ok).count() as u32;
    (mine.first().copied(), failures)
}

impl ProbeMirror {
    pub fn new(
        mcp: Arc<dyn McpStore>,
        probe: Arc<dyn ToolProbe>,
        clock: Arc<dyn Clock>,
        events: Arc<dyn Events>,
        remotes: Remotes,
        surface: SurfaceOf,
    ) -> Self {
        Self {
            mcp,
            probe,
            clock,
            events,
            remotes,
            surface,
        }
    }

    /// Every due endpoint of the repository's latest versions; `force`
    /// ignores the schedule, never the SSE rule.
    pub async fn run(
        &self,
        repository: i64,
        name: &str,
        settings: &ProbeSettings,
        force: bool,
        only: Option<i64>,
    ) -> Result<ProbeReport, StoreError> {
        let report = self.probe_due(repository, settings, force, only).await?;
        if report.answered > 0 {
            for target in self.mcp.probe_targets(repository).await? {
                if only.is_none_or(|id| id == target.version_id) {
                    super::sync::announce_drift(self.mcp.as_ref(), self.events.as_ref(), name, target.version_id, repository).await;
                }
            }
        }
        Ok(report)
    }

    async fn probe_due(&self, repository: i64, settings: &ProbeSettings, force: bool, only: Option<i64>) -> Result<ProbeReport, StoreError> {
        let now = self.clock.now();
        let mut report = ProbeReport::default();
        let mut due = Vec::new();
        for target in self.mcp.probe_targets(repository).await? {
            if only.is_some_and(|id| id != target.version_id) {
                continue;
            }
            for remote in (self.remotes)(&target.envelope_json) {
                let (last, failures) = history(&target.last_runs, &remote.url);
                if remote.sse {
                    if last.is_none() {
                        self.mcp
                            .record_probe(&ProbeRun {
                                version_id: target.version_id,
                                remote_url: remote.url.clone(),
                                protocol_version: None,
                                outcome: Err(SSE_NOT_PROBED.into()),
                                now,
                            })
                            .await?;
                        report.failed += 1;
                    }
                    report.skipped += 1;
                    continue;
                }
                let wait = settings.interval.saturating_mul(1u32 << failures.min(4));
                let fresh = last.is_some_and(|r| (now - r.ran_at).to_std().is_ok_and(|age| age < wait));
                if fresh && !force {
                    report.skipped += 1;
                    continue;
                }
                due.push((target.version_id, target.envelope_json.clone(), remote.url));
            }
        }
        let results: Vec<Result<bool, StoreError>> = futures_util::stream::iter(due)
            .map(|(version_id, envelope, url)| self.one(version_id, envelope, url, &settings.options))
            .buffer_unordered(settings.concurrency.max(1))
            .collect()
            .await;
        for r in results {
            report.probed += 1;
            if r? {
                report.answered += 1;
            } else {
                report.failed += 1;
            }
        }
        Ok(report)
    }

    async fn one(&self, version_id: i64, envelope: String, url: String, options: &ProbeOptions) -> Result<bool, StoreError> {
        let answer = self.probe.tools(&url, options).await;
        let (protocol_version, outcome) = match answer {
            Ok(a) => (Some(a.protocol_version), (self.surface)(&envelope, &url, &a.tools)),
            Err(why) => (None, Err(why)),
        };
        let ok = outcome.is_ok();
        self.mcp
            .record_probe(&ProbeRun {
                version_id,
                remote_url: url,
                protocol_version,
                outcome,
                now: self.clock.now(),
            })
            .await?;
        Ok(ok)
    }
}
