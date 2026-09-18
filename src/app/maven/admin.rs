//! What only an administrator decides for a pending unit, contested or
//! not: make it visible without its POM, or refuse it. Both are audited.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::versions::MavenVersions;
use crate::app::audit::{self, Actor};
use crate::error::{AppError, AppResult, StoreError};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::maven::{MavenFileStore, UnitChange, UnitKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Promote,
    Refuse,
}

pub struct DecideUnit {
    maven: Arc<dyn MavenFileStore>,
    versions: Arc<MavenVersions>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl DecideUnit {
    pub fn new(versions: Arc<MavenVersions>, audit: Arc<dyn AuditStore>, events: Arc<dyn Events>) -> Self {
        Self {
            maven: versions.maven.clone(),
            versions,
            audit,
            events,
        }
    }

    /// `scopes` are the metadata counters of the unit.
    pub async fn run(
        &self,
        key: &UnitKey<'_>,
        decision: Decision,
        scopes: &[String],
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let target = format!("{}:{}:{}", key.ga, key.version, key.build);
        let unit = self
            .maven
            .unit(key)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no such maven unit: {target}")))?;
        if unit.visible() {
            return Err(AppError::Conflict(format!("{target} is already published")));
        }
        if unit.refused {
            return Err(AppError::Conflict(format!("{target} was refused")));
        }
        let action = match decision {
            Decision::Promote => {
                if unit.files.is_empty() {
                    return Err(AppError::BadRequest(format!("{target} holds no file")));
                }
                let change = UnitChange {
                    key: *key,
                    revision: Some(unit.revision),
                    depositor: &unit.depositor,
                    file: None,
                    declarations: &[],
                    contest: false,
                    reveal: true,
                    scopes,
                    pins: &[],
                    now,
                };
                self.maven.change(&change).await.map_err(moved)?;
                if let Some(revealed) = self.maven.unit(key).await? {
                    if let Err(e) = self.versions.ensure(key.repository, key.ga, &revealed, now).await {
                        tracing::warn!(error = %e, "maven: promoted, version row left to the reconciler");
                    }
                }
                "maven.promote"
            }
            Decision::Refuse => {
                self.maven
                    .refuse(key, unit.revision, scopes, now)
                    .await
                    .map_err(moved)?;
                "maven.refuse"
            }
        };
        audit::record(self.audit.as_ref(), self.events.as_ref(), by, action, Some(&target), now).await;
        Ok(())
    }
}

fn moved(err: StoreError) -> AppError {
    match err {
        StoreError::Conflict => AppError::Conflict("the unit changed meanwhile, try again".to_string()),
        other => other.into(),
    }
}
