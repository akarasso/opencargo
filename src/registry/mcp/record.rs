//! The one recording point of MCP: a server version served through a
//! mirror, the moment before a client installs it. Rule enablement is the
//! member's; the facts are the addressed repository's.

use std::sync::Arc;

use super::gate::Gates;
use crate::auth::middleware::AuthUser;
use crate::domain::governance::{admits, Decision};
use crate::domain::{Drift, Format, RepoKind, Repository};
use crate::policy::facts::mcp::{Approval, McpFacts, McpFinding};
use crate::policy::{Actor, Pending, Source};
use crate::ports::mcp::{CatalogRow, SurfaceSource};
use crate::registry::resolve::Upstream;
use crate::server::AppState;

fn approval(row: &CatalogRow) -> Approval {
    let approved = row.surface_endpoints > 0 && row.approved_endpoints == row.surface_endpoints && row.worst_drift == Drift::None;
    match row.decision {
        Some(Decision::Blocked) => Approval::Blocked,
        _ if approved => Approval::Approved,
        _ => Approval::Pending,
    }
}

pub async fn served(
    state: &AppState,
    addressed: &Repository,
    member: &Repository,
    row: &CatalogRow,
    gates: &Gates,
    auth: Option<&AuthUser>,
) {
    if member.kind().ok() != Some(RepoKind::Proxy) || !state.policy.records(&member.name) {
        return;
    }
    let Ok(upstream) = Upstream::for_member(state.upstream_auth.as_ref(), member) else {
        return;
    };
    let findings = state
        .mcp
        .findings_of(row.version_id, addressed.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !f.suppressed)
        .map(|f| McpFinding {
            pattern: f.pattern,
            high: f.high,
            field: f.field,
            tool: f.tool,
        })
        .collect();
    let reviewed = state
        .mcp
        .approvals_of(&row.name, &row.version, false)
        .await
        .unwrap_or_default()
        .iter()
        .any(|a| a.repository == addressed.id || a.repository == member.id);
    let settings = state.mcp_settings.get(&addressed.name).cloned().unwrap_or_default();
    let facts = McpFacts {
        addressed: addressed.name.clone(),
        package_transports: row.package_transports.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
        remote_transports: row.remote_transports.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
        allowed: admits(&gates.addressed.rules, &row.name),
        approval: approval(row),
        reviewed,
        drift: row.worst_drift,
        drifted_remote: row.drifted_remote.clone(),
        findings,
        scan_medium: settings.scan_medium,
        tools_observed: row.current.as_ref().is_some_and(|c| c.source != SurfaceSource::Declared),
    };
    let published = row
        .published_at
        .as_deref()
        .and_then(crate::policy::facts::parse_time);
    state.policy.record(Pending {
        requested_repo: addressed.name.clone(),
        member: member.clone(),
        upstream,
        format: Format::Mcp,
        name: row.name.clone(),
        version: Some(row.version.clone()),
        actor: Actor::of(auth),
        source: Source::Mcp {
            facts: Arc::new(facts),
            digest: row.current.as_ref().map(|c| format!("sha256:{}", c.combined_sha256)),
            published,
        },
    });
}
