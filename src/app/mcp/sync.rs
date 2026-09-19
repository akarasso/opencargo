//! `SyncMirror`: page an upstream registry into a mirror's rows.
//!
//! The window of an incremental run is anchored on the previous run's own
//! start, never on the newest `updatedAt` seen: upstream orders by
//! `name:version` and filters by time, so a high water taken from the rows
//! would skip whatever changed behind the cursor during the run. The mark
//! moves only after the last page, so a failed run repeats its window.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use tracing::warn;

use crate::domain::{Audience, DomainEvent, Drift};
use crate::error::StoreError;
use crate::ports::clock::Clock;
use crate::ports::events::Events;
use crate::ports::mcp::{McpStore, RecordWrite, SyncState};
use crate::ports::mcp_feed::{FeedError, FeedQuery, RegistryFeed};

/// The wire record to the store command; the format adapter's to know.
pub type Translate = fn(i64, &Value, DateTime<Utc>) -> Result<RecordWrite, String>;

const PAGE: u32 = 100;
const MAX_PAGES: usize = 10_000;
const FULL_EVERY: Duration = Duration::days(7);
const SKEW: Duration = Duration::minutes(1);

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SyncReport {
    pub full: bool,
    pub pages: usize,
    pub upserted: usize,
    pub changed: usize,
    pub skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Feed(#[from] FeedError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Config(String),
}

pub struct SyncMirror {
    mcp: Arc<dyn McpStore>,
    feed: Arc<dyn RegistryFeed>,
    clock: Arc<dyn Clock>,
    events: Arc<dyn Events>,
    translate: Translate,
}

/// A version whose verdict moved, announced to the admins.
pub async fn announce_drift(mcp: &dyn McpStore, events: &dyn Events, repository: &str, version_id: i64, member: i64) {
    if let Ok(Some(row)) = mcp.version_by_id(version_id, member).await {
        if row.worst_drift != Drift::None {
            events.emit(
                DomainEvent::McpDrift {
                    repository: repository.to_string(),
                    server: row.name,
                    version: row.version,
                    drift: row.worst_drift.as_str().to_string(),
                },
                Audience::Admin,
            );
        }
    }
}

impl SyncMirror {
    pub fn new(
        mcp: Arc<dyn McpStore>,
        feed: Arc<dyn RegistryFeed>,
        clock: Arc<dyn Clock>,
        events: Arc<dyn Events>,
        translate: Translate,
    ) -> Self {
        Self {
            mcp,
            feed,
            clock,
            events,
            translate,
        }
    }

    /// One run. The mirror keeps serving its last good rows whatever
    /// happens; a failure is recorded with its count for the backoff.
    pub async fn run(&self, repository: i64, name: &str, upstream: &str, force_full: bool) -> Result<SyncReport, SyncError> {
        let mut state = self.mcp.sync_state(repository).await?;
        let started = self.clock.now();
        let outcome = self.pages(repository, name, upstream, &state, started, force_full).await;
        self.events.emit(
            DomainEvent::McpSynced {
                repository: name.to_string(),
                changed: outcome.as_ref().map_or(0, |r| r.changed as u64),
                failed: outcome.is_err(),
            },
            Audience::Admin,
        );
        state.last_run_at = Some(started);
        match &outcome {
            Ok(report) => {
                state.high_water = Some(started - SKEW);
                if report.full {
                    state.last_full_at = Some(started);
                }
                state.last_error = None;
                state.skipped = report.skipped as i64;
                state.consecutive_failures = 0;
            }
            Err(e) => {
                state.last_error = Some(e.to_string());
                state.consecutive_failures += 1;
            }
        }
        self.mcp.save_sync_state(repository, &state).await?;
        outcome
    }

    async fn pages(
        &self,
        repository: i64,
        name: &str,
        upstream: &str,
        state: &SyncState,
        started: DateTime<Utc>,
        force_full: bool,
    ) -> Result<SyncReport, SyncError> {
        let base = url::Url::parse(upstream).map_err(|e| SyncError::Config(format!("invalid upstream: {e}")))?;
        let full = force_full
            || state.high_water.is_none()
            || state.last_full_at.is_none_or(|at| started - at > FULL_EVERY);
        let mut report = SyncReport {
            full,
            ..SyncReport::default()
        };
        let mut query = FeedQuery {
            cursor: None,
            updated_since: if full { None } else { state.high_water },
            limit: PAGE,
            include_deleted: true,
        };
        for _ in 0..MAX_PAGES {
            let page = self.feed.page(&base, &query).await?;
            report.pages += 1;
            for envelope in &page.servers {
                match (self.translate)(repository, envelope, self.clock.now()) {
                    Ok(write) => {
                        let done = self.mcp.upsert_record(&write).await?;
                        report.upserted += 1;
                        report.changed += usize::from(done.changed);
                        if done.changed {
                            announce_drift(self.mcp.as_ref(), self.events.as_ref(), name, done.version_id, repository).await;
                        }
                    }
                    Err(why) => {
                        let name = envelope.pointer("/server/name").and_then(Value::as_str).unwrap_or("?");
                        warn!(repository, server = name, error = %why, "skipping an unreadable MCP record");
                        report.skipped += 1;
                    }
                }
            }
            match page.next_cursor {
                Some(next) if !page.servers.is_empty() => query.cursor = Some(next),
                _ => return Ok(report),
            }
        }
        Err(SyncError::Config("the upstream cursor never ended".into()))
    }
}
