//! What every format does either side of its own publish: the gate that runs
//! before the first write, and the tail that runs once the version is
//! serveable.
//!
//! The tail is what turns a stored version into an announced one: the
//! webhook, the real-time event under the audience `app::events` decides,
//! and the scan record.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::app::events::Announce;
use crate::app::scan::ScanVersion;
use crate::config::VulnScanConfig;
use crate::domain::{DomainEvent, Format, PackageRelease, ScanResult};
use crate::error::{AppError, AppResult};
use crate::ports::vulns::{ScanError, VulnFeed, VulnStore};
use crate::telemetry::webhooks::WebhookDispatcher;

/// The scan that ran before the first write, if any; persisted by
/// [`PublishTail::run`] once the version row exists.
#[derive(Debug, Default)]
pub struct PreScan(Option<ScanResult>);

/// Refuses a publish whose dependencies are already known to be critical,
/// before any file or row is written.
pub struct PublishGate {
    feed: Arc<dyn VulnFeed>,
    config: VulnScanConfig,
}

impl PublishGate {
    pub fn new(feed: Arc<dyn VulnFeed>, config: VulnScanConfig) -> Self {
        Self { feed, config }
    }

    /// With `block_on_critical` a critical finding refuses the publish; an
    /// OSV outage follows `fail_closed`.
    pub async fn run(&self, format: Format, metadata_json: &str) -> AppResult<PreScan> {
        if format.osv_ecosystem().is_none() {
            return Ok(PreScan(None));
        }
        if !(self.config.enabled && self.config.block_on_critical) {
            return Ok(PreScan(None));
        }
        match self.feed.assess(metadata_json, format).await {
            Ok(result) if result.status == "critical" => Err(AppError::BadRequest(
                "publish blocked: critical vulnerabilities found in dependencies".to_string(),
            )),
            Ok(result) => Ok(PreScan(Some(result))),
            Err(ScanError::Unscannable(why)) => {
                warn!(%why, "dependencies unknown; publishing unscanned");
                Ok(PreScan(None))
            }
            Err(e) if self.config.fail_closed => Err(AppError::ServiceUnavailable(format!(
                "vulnerability scan unavailable: {e}"
            ))),
            Err(e) => {
                warn!(error = %e, "vulnerability scan unavailable; publishing unscanned");
                Ok(PreScan(None))
            }
        }
    }
}

/// One version that is now serveable, as the tail needs to describe it.
pub struct Published<'a> {
    pub format: Format,
    pub repository: &'a str,
    pub package: &'a str,
    pub version: &'a str,
    /// `None` for the formats with no `versions` row (OCI), which have
    /// nothing to scan.
    pub version_id: Option<i64>,
    pub metadata_json: &'a str,
    pub published_by: &'a str,
}

/// The shared post-publish side effects, over the ports they reach the world
/// through: the webhook, the real-time event and its audience, then the scan.
pub struct PublishTail {
    announce: Announce,
    webhooks: Arc<WebhookDispatcher>,
    feed: Arc<dyn VulnFeed>,
    vulns: Arc<dyn VulnStore>,
}

impl PublishTail {
    pub fn new(
        announce: Announce,
        webhooks: Arc<WebhookDispatcher>,
        feed: Arc<dyn VulnFeed>,
        vulns: Arc<dyn VulnStore>,
    ) -> Self {
        Self {
            announce,
            webhooks,
            feed,
            vulns,
        }
    }

    /// The version is already served, so nothing here can refuse it: every
    /// step is best-effort and says so in the log.
    pub async fn run(&self, done: &Published<'_>, pre: PreScan, now: DateTime<Utc>) {
        crate::telemetry::record_publish(done.repository, done.package);
        self.webhooks
            .dispatch(
                "package.published",
                &serde_json::json!({
                    "package": done.package,
                    "version": done.version,
                    "repository": done.repository,
                    "published_by": done.published_by,
                }),
            )
            .await;

        let event = DomainEvent::PackagePublished(PackageRelease {
            package: done.package.to_string(),
            version: done.version.to_string(),
            repository: done.repository.to_string(),
            format: done.format,
            published_by: done.published_by.to_string(),
        });
        self.announce.package_event(event, done.repository).await;
        self.scan(done, pre, now).await;
    }

    /// Persist the pre-publish scan, or run one in the background when the
    /// gate did not.
    async fn scan(&self, done: &Published<'_>, pre: PreScan, now: DateTime<Utc>) {
        let Some(version_id) = done.version_id.filter(|_| done.format.osv_ecosystem().is_some())
        else {
            return;
        };
        let scan = ScanVersion::new(self.feed.clone(), self.vulns.clone());
        match pre.0 {
            // The version is already served; a lost scan row is a warning, not a failed publish.
            Some(result) => {
                if let Err(e) = scan.record(version_id, &result, now).await {
                    warn!(version_id, error = %e, "failed to persist the pre-publish scan");
                }
            }
            None => {
                let meta_json = done.metadata_json.to_string();
                let format = done.format;
                tokio::spawn(async move {
                    match scan.run(version_id, &meta_json, format, Utc::now()).await {
                        Ok(_) => {}
                        Err(ScanError::Unscannable(why)) => {
                            warn!(version_id, %why, "version not scanned: no scan row will say it was")
                        }
                        Err(e) => warn!(error = %e, "Background vulnerability scan failed"),
                    }
                });
            }
        }
    }
}
