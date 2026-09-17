pub mod deps;
pub mod osv;
pub mod severity;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::config::VulnScanConfig;
use osv::{Advisory, OsvClient};
use severity::Severity;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("OSV query failed: {0}")]
    Upstream(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// `osv` is `None` while scanning is disabled: assess reports clean, nothing is recorded.
#[derive(Clone)]
pub struct VulnScanner {
    osv: Option<OsvClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub total_deps: usize,
    pub vulnerable_deps: usize,
    pub status: String,
    pub details: Vec<VulnDetail>,
}

impl ScanResult {
    fn clean() -> Self {
        Self {
            total_deps: 0,
            vulnerable_deps: 0,
            status: "clean".to_string(),
            details: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulnDetail {
    pub dependency: String,
    pub version: String,
    pub vuln_id: String,
    pub summary: String,
    pub severity: Severity,
    pub score: Option<f64>,
}

impl VulnDetail {
    fn new(dependency: &str, version: &str, id: &str, advisory: Option<&Advisory>) -> Self {
        Self {
            dependency: dependency.to_string(),
            version: version.to_string(),
            vuln_id: id.to_string(),
            summary: advisory.and_then(|a| a.summary.clone()).unwrap_or_default(),
            severity: advisory.map_or(Severity::Unknown, |a| a.severity),
            score: advisory.and_then(|a| a.score),
        }
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

    pub fn enabled(&self) -> bool {
        self.osv.is_some()
    }

    /// The advisories of each `(name, version)` itself, one `querybatch`
    /// under the scanner's permit; a disabled scanner finds nothing.
    pub async fn assess_batch(
        &self,
        ecosystem: &str,
        deps: &[(String, String)],
    ) -> Result<Vec<Vec<VulnDetail>>, ScanError> {
        let Some(osv) = &self.osv else {
            return Ok(vec![Vec::new(); deps.len()]);
        };
        let hits = osv.query_batch(ecosystem, deps).await?;
        let ids: Vec<String> = hits
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let advisories = osv.advisories(&ids).await;
        Ok(details_per_dep(deps, &hits, &advisories))
    }

    /// Query OSV for the dependencies of `metadata_json`; pure, no DB.
    pub async fn assess(
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
        let ids: Vec<String> = hits
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let advisories = osv.advisories(&ids).await;
        Ok(summarize(&deps, &hits, &advisories))
    }

    /// Record a scan result against a version row.
    pub async fn persist(
        &self,
        db: &SqlitePool,
        version_id: i64,
        r: &ScanResult,
    ) -> Result<(), sqlx::Error> {
        let results_json =
            serde_json::to_string(r).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        crate::db::insert_vulnerability_scan(
            db,
            version_id,
            r.total_deps as i64,
            r.vulnerable_deps as i64,
            Some(&results_json),
            &r.status,
        )
        .await?;
        info!(
            version_id,
            total_deps = r.total_deps,
            vulnerable_deps = r.vulnerable_deps,
            status = %r.status,
            "Vulnerability scan completed"
        );
        Ok(())
    }

    /// `assess` then `persist`; a disabled scanner reports clean and records nothing.
    pub async fn scan_version(
        &self,
        db: &SqlitePool,
        version_id: i64,
        metadata_json: &str,
        ecosystem: &str,
    ) -> Result<ScanResult, ScanError> {
        let result = self.assess(metadata_json, ecosystem).await?;
        if self.osv.is_some() {
            self.persist(db, version_id, &result).await?;
        }
        Ok(result)
    }
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
                    VulnDetail::new(name, version, id, advisory)
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
