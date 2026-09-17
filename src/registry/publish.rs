use tracing::warn;

use crate::db::kinds::Format;
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::telemetry::vulns::ScanResult;

/// The scan that ran before the first write, if any; persisted by
/// [`finalize_publish`] once the version row exists.
#[derive(Debug, Default)]
pub struct PreScan(Option<ScanResult>);

/// Runs before any file or row is written: with `block_on_critical` a
/// critical finding refuses the publish, an OSV outage follows `fail_closed`.
pub async fn publish_gate(
    state: &AppState,
    format: Format,
    metadata_json: &str,
) -> AppResult<PreScan> {
    let cfg = &state.vuln_scan_config;
    let Some(ecosystem) = format.osv_ecosystem() else {
        return Ok(PreScan(None));
    };
    if !(cfg.enabled && cfg.block_on_critical) {
        return Ok(PreScan(None));
    }
    match state.vuln_scanner.assess(metadata_json, ecosystem).await {
        Ok(result) if result.status == "critical" => Err(AppError::BadRequest(
            "publish blocked: critical vulnerabilities found in dependencies".to_string(),
        )),
        Ok(result) => Ok(PreScan(Some(result))),
        Err(e) if cfg.fail_closed => Err(AppError::ServiceUnavailable(format!(
            "vulnerability scan unavailable: {e}"
        ))),
        Err(e) => {
            warn!(error = %e, "vulnerability scan unavailable; publishing unscanned");
            Ok(PreScan(None))
        }
    }
}

/// Shared post-publish side effects: the `package.published` webhook, the
/// real-time event, then the scan result (persisted when the gate ran, else
/// scanned in the background). `version_id` is `None` for formats without a
/// `versions` row (OCI), which have nothing to scan.
#[allow(clippy::too_many_arguments)]
pub async fn finalize_publish(
    state: &AppState,
    format: Format,
    repo_name: &str,
    package_name: &str,
    version_str: &str,
    version_id: Option<i64>,
    metadata_json: &str,
    published_by: &str,
    pre: PreScan,
) -> AppResult<()> {
    state
        .webhook_dispatcher
        .dispatch(
            "package.published",
            &serde_json::json!({
                "package": package_name,
                "version": version_str,
                "repository": repo_name,
                "published_by": published_by,
            }),
        )
        .await;

    super::emit_package_event(
        state,
        "package.published",
        repo_name,
        serde_json::json!({
            "package": package_name,
            "version": version_str,
            "repository": repo_name,
            "format": format.as_str(),
            "published_by": published_by,
        }),
    )
    .await;

    let (Some(version_id), Some(ecosystem)) = (version_id, format.osv_ecosystem()) else {
        return Ok(());
    };

    match pre.0 {
        Some(result) => {
            // The version is already served; a lost scan row is a warning, not a failed publish.
            if let Err(e) = state.vuln_scanner.persist(&state.db, version_id, &result).await {
                warn!(version_id, error = %e, "failed to persist the pre-publish scan");
            }
        }
        None => {
            let scanner = state.vuln_scanner.clone();
            let db = state.db.clone();
            let meta_json = metadata_json.to_string();
            let eco = ecosystem.to_string();
            tokio::spawn(async move {
                if let Err(e) = scanner.scan_version(&db, version_id, &meta_json, &eco).await {
                    warn!(error = %e, "Background vulnerability scan failed");
                }
            });
        }
    }
    Ok(())
}
