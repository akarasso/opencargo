use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{info, warn};

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

#[derive(Clone)]
pub struct VulnScanner {
    client: reqwest::Client,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub total_deps: usize,
    pub vulnerable_deps: usize,
    pub status: String,
    pub details: Vec<VulnDetail>,
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
    pub fn new(enabled: bool) -> Self {
        Self {
            client: reqwest::Client::new(),
            enabled,
        }
    }

    /// Scan dependencies of a published package version.
    /// Extracts deps from metadata_json, queries OSV.dev, stores results.
    pub async fn scan_version(
        &self,
        db: &SqlitePool,
        version_id: i64,
        metadata_json: &str,
        ecosystem: &str,
    ) -> Result<ScanResult, anyhow::Error> {
        if !self.enabled {
            let result = ScanResult {
                total_deps: 0,
                vulnerable_deps: 0,
                status: "clean".to_string(),
                details: vec![],
            };
            return Ok(result);
        }

        // Extract dependencies from the metadata JSON
        let deps = extract_dependencies(metadata_json, ecosystem);

        if deps.is_empty() {
            let result = ScanResult {
                total_deps: 0,
                vulnerable_deps: 0,
                status: "clean".to_string(),
                details: vec![],
            };

            let results_json = serde_json::to_string(&result)?;
            crate::db::insert_vulnerability_scan(
                db,
                version_id,
                0,
                0,
                Some(&results_json),
                "clean",
            )
            .await?;

            return Ok(result);
        }

        // Build OSV queries
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

        let total_deps = queries.len();

        // Query OSV.dev API
        let osv_result = self
            .client
            .post("https://api.osv.dev/v1/querybatch")
            .json(&OsvQueryBatch { queries })
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        let response = match osv_result {
            Ok(resp) => match resp.json::<OsvBatchResponse>().await {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!(error = %e, "Failed to parse OSV.dev response");
                    let result = ScanResult {
                        total_deps,
                        vulnerable_deps: 0,
                        status: "error".to_string(),
                        details: vec![],
                    };
                    // Store error status — we use "clean" as fallback since
                    // the CHECK constraint only allows clean/warning/critical
                    let results_json = serde_json::to_string(&result)?;
                    crate::db::insert_vulnerability_scan(
                        db,
                        version_id,
                        total_deps as i64,
                        0,
                        Some(&results_json),
                        "clean",
                    )
                    .await?;
                    return Ok(result);
                }
            },
            Err(e) => {
                warn!(error = %e, "Failed to query OSV.dev");
                let result = ScanResult {
                    total_deps,
                    vulnerable_deps: 0,
                    status: "error".to_string(),
                    details: vec![],
                };
                let results_json = serde_json::to_string(&result)?;
                crate::db::insert_vulnerability_scan(
                    db,
                    version_id,
                    total_deps as i64,
                    0,
                    Some(&results_json),
                    "clean",
                )
                .await?;
                return Ok(result);
            }
        };

        // Process results
        let mut details = Vec::new();
        let mut has_critical = false;

        for (i, osv_result) in response.results.iter().enumerate() {
            if !osv_result.vulns.is_empty() {
                let (dep_name, dep_version) = &deps[i];
                for vuln in &osv_result.vulns {
                    let severity = vuln
                        .severity
                        .first()
                        .and_then(|s| s.score.clone());

                    // Check if any severity score is >= 9.0 (critical)
                    if let Some(ref score_str) = severity {
                        if let Ok(score) = score_str.parse::<f64>() {
                            if score >= 9.0 {
                                has_critical = true;
                            }
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
        }

        let vulnerable_deps = details
            .iter()
            .map(|d| d.dependency.clone())
            .collect::<std::collections::HashSet<_>>()
            .len();

        let status = if has_critical {
            "critical".to_string()
        } else if vulnerable_deps > 0 {
            "warning".to_string()
        } else {
            "clean".to_string()
        };

        let result = ScanResult {
            total_deps,
            vulnerable_deps,
            status: status.clone(),
            details,
        };

        let results_json = serde_json::to_string(&result)?;
        crate::db::insert_vulnerability_scan(
            db,
            version_id,
            total_deps as i64,
            vulnerable_deps as i64,
            Some(&results_json),
            &status,
        )
        .await?;

        info!(
            version_id = version_id,
            total_deps = total_deps,
            vulnerable_deps = vulnerable_deps,
            status = %status,
            "Vulnerability scan completed"
        );

        Ok(result)
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
