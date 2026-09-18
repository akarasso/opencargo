//! `DecideVersion` and `DecideSkill`: an admin's approval or block, taken
//! against the exact surfaces a repository serves and audited.
//!
//! A server version is decided once per endpoint of its current set, in one
//! transaction: every observed endpoint, or the declared slot while none is,
//! each bound to the record's permissions and that endpoint's tools.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::app::audit::{record, Actor};
use crate::domain::governance::Decision;
use crate::error::StoreError;
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::mcp::{CatalogRow, McpStore, NewApproval, SkillRow, SurfaceRow, SurfaceSource};

pub struct DecideVersion {
    mcp: Arc<dyn McpStore>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

/// Each endpoint's best surface, in the order the store ranks them.
fn endpoints(surfaces: &[SurfaceRow]) -> Vec<&SurfaceRow> {
    let observed: Vec<&SurfaceRow> = surfaces.iter().filter(|s| s.source != SurfaceSource::Declared).collect();
    if observed.is_empty() {
        return surfaces.iter().filter(|s| s.source == SurfaceSource::Declared).take(1).collect();
    }
    let mut out: Vec<&SurfaceRow> = Vec::new();
    for s in observed {
        if !out.iter().any(|o| o.remote_url == s.remote_url) {
            out.push(s);
        }
    }
    out
}

impl DecideVersion {
    pub fn new(mcp: Arc<dyn McpStore>, audit: Arc<dyn AuditStore>, events: Arc<dyn Events>) -> Self {
        Self { mcp, audit, events }
    }

    pub async fn server(
        &self,
        addressed: i64,
        row: &CatalogRow,
        decision: Decision,
        note: Option<String>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> Result<usize, StoreError> {
        let surfaces = self.mcp.surfaces(row.version_id).await?;
        let declared = surfaces
            .iter()
            .find(|s| s.source == SurfaceSource::Declared)
            .ok_or(StoreError::NotFound)?;
        let approvals: Vec<NewApproval> = endpoints(&surfaces)
            .into_iter()
            .map(|s| NewApproval {
                repository: addressed,
                skill: false,
                name: row.name.clone(),
                version: row.version.clone(),
                remote_url: s.remote_url.clone(),
                permissions_sha256: declared.permissions_sha256.clone(),
                tools_sha256: s.tools_sha256.clone(),
                combined_sha256: s.combined_sha256.clone(),
                surface_id: Some(s.id),
                decision,
                decided_by: by.username.to_string(),
                note: note.clone(),
                now,
            })
            .collect();
        self.mcp.decide(&approvals).await?;
        let action = if decision == Decision::Approved { "mcp.approve" } else { "mcp.block" };
        record(&*self.audit, &*self.events, by, action, Some(&format!("{}@{}", row.name, row.version)), now).await;
        Ok(approvals.len())
    }

    pub async fn skill(
        &self,
        addressed: i64,
        skill: &SkillRow,
        decision: Decision,
        note: Option<String>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.mcp
            .decide(&[NewApproval {
                repository: addressed,
                skill: true,
                name: skill.name.clone(),
                version: skill.version.clone(),
                remote_url: String::new(),
                permissions_sha256: skill.surface_sha256.clone(),
                tools_sha256: None,
                combined_sha256: skill.surface_sha256.clone(),
                surface_id: None,
                decision,
                decided_by: by.username.to_string(),
                note,
                now,
            }])
            .await?;
        let action = if decision == Decision::Approved { "mcp.skill.approve" } else { "mcp.skill.block" };
        record(&*self.audit, &*self.events, by, action, Some(&format!("{}@{}", skill.name, skill.version)), now).await;
        Ok(())
    }
}
