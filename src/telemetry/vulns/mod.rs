pub mod deps;
pub mod osv;
pub mod severity;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use tracing::warn;

use crate::config::VulnScanConfig;
use crate::domain::{ScanResult, Severity, VulnDetail};
use crate::ports::vulns::{ScanError, VulnFeed};
use osv::{Advisory, OsvClient};

/// `osv` is `None` while scanning is disabled: assess reports clean, nothing is recorded.
#[derive(Clone)]
pub struct VulnScanner {
    osv: Option<OsvClient>,
}

/// One detail the feed produced, which the domain owns as a value.
fn detail(dependency: &str, version: &str, id: &str, advisory: Option<&Advisory>) -> VulnDetail {
    VulnDetail {
        dependency: dependency.to_string(),
        version: version.to_string(),
        vuln_id: id.to_string(),
        summary: advisory.and_then(|a| a.summary.clone()).unwrap_or_default(),
        severity: advisory.map_or(Severity::Unknown, |a| a.severity),
        score: advisory.and_then(|a| a.score),
    }
}

impl VulnScanner {
    pub fn new(cfg: &VulnScanConfig) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(&cfg.osv_base_url)
            .with_context(|| format!("invalid vuln_scan.osv_base_url: {}", cfg.osv_base_url))?;
        Ok(Self {
            osv: cfg
                .enabled
                .then(|| OsvClient::new(base, cfg.max_concurrency)),
        })
    }
}

#[async_trait]
impl VulnFeed for VulnScanner {
    fn enabled(&self) -> bool {
        self.osv.is_some()
    }

    async fn assess_batch(
        &self,
        ecosystem: &str,
        deps: &[(String, String)],
    ) -> Result<Vec<Vec<VulnDetail>>, ScanError> {
        let Some(osv) = &self.osv else {
            return Ok(vec![Vec::new(); deps.len()]);
        };
        let hits = osv.query_batch(ecosystem, deps).await?;
        let advisories = osv.advisories(&ids_of(&hits)).await;
        Ok(details_per_dep(deps, &hits, &advisories))
    }

    async fn assess(
        &self,
        metadata_json: &str,
        ecosystem: &str,
    ) -> Result<ScanResult, ScanError> {
        let Some(osv) = &self.osv else {
            return Ok(ScanResult::clean());
        };
        let deps = deps::extract_dependencies(metadata_json, ecosystem);
        if deps.is_empty() {
            return Ok(ScanResult::clean());
        }
        let hits = osv.query_batch(ecosystem, &deps).await?;
        let advisories = osv.advisories(&ids_of(&hits)).await;
        Ok(summarize(&deps, &hits, &advisories))
    }
}

/// The advisory ids a batch of hits names, each one once.
fn ids_of(hits: &[Vec<String>]) -> Vec<String> {
    hits.iter()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// One detail per (dependency, advisory); a record that could not be fetched
/// is still a finding, of unknown severity.
fn details_per_dep(
    deps: &[(String, String)],
    hits: &[Vec<String>],
    advisories: &HashMap<String, Result<Arc<Advisory>, String>>,
) -> Vec<Vec<VulnDetail>> {
    deps.iter()
        .zip(hits)
        .map(|((name, version), ids)| {
            ids.iter()
                .map(|id| {
                    let advisory = match advisories.get(id) {
                        Some(Ok(a)) => Some(a.as_ref()),
                        Some(Err(e)) => {
                            warn!(advisory = %id, error = %e, "advisory record unavailable");
                            None
                        }
                        None => None,
                    };
                    detail(name, version, id, advisory)
                })
                .collect()
        })
        .collect()
}

fn summarize(
    deps: &[(String, String)],
    hits: &[Vec<String>],
    advisories: &HashMap<String, Result<Arc<Advisory>, String>>,
) -> ScanResult {
    let details: Vec<VulnDetail> = details_per_dep(deps, hits, advisories)
        .into_iter()
        .flatten()
        .collect();
    let vulnerable_deps = details
        .iter()
        .map(|d| d.dependency.as_str())
        .collect::<HashSet<_>>()
        .len();
    let status = if details.iter().any(|d| d.severity == Severity::Critical) {
        "critical"
    } else if details.is_empty() {
        "clean"
    } else {
        "warning"
    };
    ScanResult {
        total_deps: deps.len(),
        vulnerable_deps,
        status: status.to_string(),
        details,
    }
}
