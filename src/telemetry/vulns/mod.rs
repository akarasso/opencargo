use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::info;

use crate::config::VulnScanConfig;

// ---------------------------------------------------------------------------
// OSV.dev API types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OsvQueryBatch {
    queries: Vec<OsvQuery>,
}

#[derive(Debug, Serialize)]
struct OsvQuery {
    package: OsvPackage,
    version: String,
}

#[derive(Debug, Serialize)]
struct OsvPackage {
    name: String,
    ecosystem: String,
}

#[derive(Debug, Deserialize)]
struct OsvBatchResponse {
    results: Vec<OsvResult>,
}

#[derive(Debug, Deserialize)]
struct OsvResult {
    #[serde(default)]
    vulns: Vec<OsvVuln>,
}

#[derive(Debug, Deserialize)]
struct OsvVuln {
    id: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    severity: Vec<OsvSeverity>,
}

#[derive(Debug, Deserialize)]
struct OsvSeverity {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    severity_type: Option<String>,
    score: Option<String>,
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("OSV query failed: {0}")]
    Upstream(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct VulnScanner {
    client: reqwest::Client,
    base: reqwest::Url,
    enabled: bool,
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
    pub severity: Option<String>,
}

impl VulnScanner {
    pub fn new(cfg: &VulnScanConfig) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(&cfg.osv_base_url)
            .with_context(|| format!("invalid vuln_scan.osv_base_url: {}", cfg.osv_base_url))?;
        Ok(Self {
            client: reqwest::Client::new(),
            base,
            enabled: cfg.enabled,
        })
    }

    /// Query OSV for the dependencies of `metadata_json`; pure, no DB.
    pub async fn assess(
        &self,
        metadata_json: &str,
        ecosystem: &str,
    ) -> Result<ScanResult, ScanError> {
        if !self.enabled {
            return Ok(ScanResult::clean());
        }
        let deps = extract_dependencies(metadata_json, ecosystem);
        if deps.is_empty() {
            return Ok(ScanResult::clean());
        }
        let queries: Vec<OsvQuery> = deps
            .iter()
            .map(|(name, version)| OsvQuery {
                package: OsvPackage {
                    name: name.clone(),
                    ecosystem: ecosystem.to_string(),
                },
                version: version.clone(),
            })
            .collect();
        let url = format!("{}/v1/querybatch", self.base.as_str().trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .json(&OsvQueryBatch { queries })
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| ScanError::Upstream(format!("request failed: {e}")))?
            .json::<OsvBatchResponse>()
            .await
            .map_err(|e| ScanError::Upstream(format!("invalid response: {e}")))?;
        Ok(summarize(&deps, &response))
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
        if self.enabled {
            self.persist(db, version_id, &result).await?;
        }
        Ok(result)
    }
}

/// Fold the OSV batch response into a result; a score of 9.0 or more is critical.
fn summarize(deps: &[(String, String)], response: &OsvBatchResponse) -> ScanResult {
    let mut details = Vec::new();
    let mut has_critical = false;

    for (i, osv_result) in response.results.iter().enumerate() {
        // The OSV response is third-party controlled; never index `deps` by its length.
        let Some((dep_name, dep_version)) = deps.get(i) else {
            continue;
        };
        for vuln in &osv_result.vulns {
            let severity = vuln.severity.first().and_then(|s| s.score.clone());
            if let Some(score) = severity.as_deref().and_then(|s| s.parse::<f64>().ok()) {
                if score >= 9.0 {
                    has_critical = true;
                }
            }
            details.push(VulnDetail {
                dependency: dep_name.clone(),
                version: dep_version.clone(),
                vuln_id: vuln.id.clone(),
                summary: vuln.summary.clone().unwrap_or_default(),
                severity,
            });
        }
    }

    let vulnerable_deps = details
        .iter()
        .map(|d| d.dependency.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let status = if has_critical {
        "critical"
    } else if vulnerable_deps > 0 {
        "warning"
    } else {
        "clean"
    };
    ScanResult {
        total_deps: deps.len(),
        vulnerable_deps,
        status: status.to_string(),
        details,
    }
}

/// Extract dependency name/version pairs from metadata JSON.
///
/// For npm: look at "dependencies", "devDependencies", etc.
/// For cargo (crates.io): look at "deps" array.
/// For Go: look at "dependencies".
fn extract_dependencies(metadata_json: &str, ecosystem: &str) -> Vec<(String, String)> {
    let meta: serde_json::Value = match serde_json::from_str(metadata_json) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut deps = Vec::new();

    match ecosystem {
        "npm" => {
            // npm metadata stores dependencies as {"name": "version_req"}
            let fields = [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ];
            for field in &fields {
                if let Some(obj) = meta.get(*field).and_then(|v| v.as_object()) {
                    for (name, version) in obj {
                        let version_str = version.as_str().unwrap_or("*");
                        // Only use exact versions for OSV queries (strip ^, ~, etc.)
                        let clean_version = clean_version_string(version_str);
                        if !clean_version.is_empty() {
                            deps.push((name.clone(), clean_version));
                        }
                    }
                }
            }
        }
        "crates.io" => {
            // Cargo metadata stores deps as an array of objects
            if let Some(deps_array) = meta.get("deps").and_then(|v| v.as_array()) {
                for dep in deps_array {
                    let name = dep.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let version_req = dep
                        .get("version_req")
                        .and_then(|v| v.as_str())
                        .unwrap_or("*");
                    let clean = clean_version_string(version_req);
                    if !name.is_empty() && !clean.is_empty() {
                        deps.push((name.to_string(), clean));
                    }
                }
            }
        }
        "Go" => {
            // Go module metadata
            if let Some(obj) = meta.get("dependencies").and_then(|v| v.as_object()) {
                for (name, version) in obj {
                    let version_str = version.as_str().unwrap_or("");
                    if !version_str.is_empty() {
                        deps.push((name.clone(), version_str.to_string()));
                    }
                }
            }
        }
        _ => {}
    }

    deps
}

/// Clean a version string by removing common range prefixes.
/// OSV.dev needs exact versions, not ranges.
fn clean_version_string(version: &str) -> String {
    let v = version.trim();
    // Strip ^, ~, >=, <=, >, <, = prefixes
    let v = v.trim_start_matches('^');
    let v = v.trim_start_matches('~');
    let v = v.trim_start_matches(">=");
    let v = v.trim_start_matches("<=");
    let v = v.trim_start_matches('>');
    let v = v.trim_start_matches('<');
    let v = v.trim_start_matches('=');
    let v = v.trim();

    // Skip wildcards and complex ranges
    if v == "*" || v.contains("||") || v.contains(' ') {
        return String::new();
    }

    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_npm_dependencies() {
        let meta = r#"{
            "name": "test-pkg",
            "version": "1.0.0",
            "dependencies": {
                "lodash": "^4.17.20",
                "axios": "~0.21.0"
            },
            "devDependencies": {
                "jest": "^27.0.0"
            }
        }"#;

        let deps = extract_dependencies(meta, "npm");
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().any(|(n, v)| n == "lodash" && v == "4.17.20"));
        assert!(deps.iter().any(|(n, v)| n == "axios" && v == "0.21.0"));
        assert!(deps.iter().any(|(n, v)| n == "jest" && v == "27.0.0"));
    }

    #[test]
    fn test_extract_cargo_dependencies() {
        let meta = r#"{
            "name": "my-crate",
            "vers": "0.1.0",
            "deps": [
                {"name": "serde", "version_req": "^1.0"},
                {"name": "tokio", "version_req": ">=1.0"}
            ]
        }"#;

        let deps = extract_dependencies(meta, "crates.io");
        assert_eq!(deps.len(), 2);
        assert!(deps.iter().any(|(n, v)| n == "serde" && v == "1.0"));
        assert!(deps.iter().any(|(n, v)| n == "tokio" && v == "1.0"));
    }

    #[test]
    fn test_clean_version_string() {
        assert_eq!(clean_version_string("^4.17.20"), "4.17.20");
        assert_eq!(clean_version_string("~0.21.0"), "0.21.0");
        assert_eq!(clean_version_string(">=1.0.0"), "1.0.0");
        assert_eq!(clean_version_string("*"), "");
        assert_eq!(clean_version_string("1.0.0 || 2.0.0"), "");
    }

    #[test]
    fn test_extract_no_deps() {
        let meta = r#"{"name": "empty", "version": "1.0.0"}"#;
        let deps = extract_dependencies(meta, "npm");
        assert!(deps.is_empty());
    }
}
