//! `MavenReconcile`: what `RunCleanup` repairs for Maven. A pending unit
//! past the window is promoted when nothing contests it and no declaration
//! waits; a visible version without its `versions` row gets one, announced
//! once. The client's own metadata needs no repair: it is never served, and
//! only its hints naming a visible version are rendered.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::rules::promotable;
use super::versions::{MavenVersions, Versioned};
use crate::app::reconcile::{Item, Reconciled, Reconciler};
use crate::error::StoreError;
use crate::ports::maven::{MavenFileStore, PendingUnit, UnitChange, UnitKey, Unversioned};

pub struct MavenReconcile {
    maven: Arc<dyn MavenFileStore>,
    versions: Arc<MavenVersions>,
    window: chrono::Duration,
    limit: u32,
    /// Where the last pass stopped scanning unversioned values.
    cursor: Mutex<Option<Unversioned>>,
    /// The metadata counters of a unit, as the protocol adapter scopes them.
    scopes: fn(&str, &str) -> Vec<String>,
}

impl MavenReconcile {
    pub fn new(
        versions: Arc<MavenVersions>,
        window: chrono::Duration,
        limit: u32,
        scopes: fn(&str, &str) -> Vec<String>,
    ) -> Self {
        Self {
            maven: versions.maven.clone(),
            versions,
            window,
            limit,
            cursor: Mutex::new(None),
            scopes,
        }
    }

    async fn promote(&self, p: &PendingUnit, now: DateTime<Utc>) -> Reconciled {
        let key = UnitKey {
            repository: p.repository,
            ga: &p.ga,
            version: &p.version,
            build: &p.build,
        };
        let unit = match self.maven.unit(&key).await {
            Ok(Some(unit)) => unit,
            Ok(None) => return Reconciled::Clean,
            Err(e) => return Reconciled::Failed(e.to_string()),
        };
        if unit.contested {
            return Reconciled::Skipped("contested: an administrator decides".to_string());
        }
        if !promotable(&unit, now, self.window) {
            return Reconciled::Clean;
        }
        let scopes = (self.scopes)(&p.ga, &p.version);
        let change = UnitChange {
            key,
            revision: Some(unit.revision),
            depositor: &unit.depositor,
            file: None,
            declarations: &[],
            contest: false,
            reveal: true,
            scopes: &scopes,
            pins: &[],
            now,
        };
        match self.maven.change(&change).await {
            Ok(_) => {}
            Err(StoreError::Conflict) => return Reconciled::Skipped("the unit moved".to_string()),
            Err(e) => return Reconciled::Failed(e.to_string()),
        }
        match self.maven.unit(&key).await {
            Ok(Some(revealed)) => match self.versions.ensure(p.repository, &p.ga, &revealed, now).await {
                Ok(_) => Reconciled::Repaired,
                Err(e) => Reconciled::Failed(format!("promoted, version row left to the next pass: {e}")),
            },
            Ok(None) => Reconciled::Repaired,
            Err(e) => Reconciled::Failed(e.to_string()),
        }
    }

    async fn version(&self, u: &Unversioned, now: DateTime<Utc>) -> Reconciled {
        let units = match self.maven.artifact(u.repository, &u.ga).await {
            Ok(units) => units,
            Err(e) => return Reconciled::Failed(e.to_string()),
        };
        let Some(view) = units.iter().find(|v| v.version == u.version && v.visible()) else {
            return Reconciled::Clean;
        };
        let key = UnitKey {
            repository: u.repository,
            ga: &u.ga,
            version: &u.version,
            build: &view.build,
        };
        let unit = match self.maven.unit(&key).await {
            Ok(Some(unit)) => unit,
            Ok(None) => return Reconciled::Clean,
            Err(e) => return Reconciled::Failed(e.to_string()),
        };
        match self.versions.ensure(u.repository, &u.ga, &unit, now).await {
            Ok(Versioned::Published) => Reconciled::Repaired,
            Ok(Versioned::Already) => Reconciled::Clean,
            Err(e) => Reconciled::Failed(e.to_string()),
        }
    }
}

#[async_trait]
impl Reconciler for MavenReconcile {
    fn name(&self) -> &'static str {
        "maven"
    }

    async fn pass(&self, now: DateTime<Utc>) -> Vec<Item> {
        let mut items = Vec::new();
        match self.maven.pending(now - self.window, self.limit).await {
            Ok(pending) => {
                for p in &pending {
                    items.push(Item {
                        name: format!("promote {}:{}:{}", p.ga, p.version, p.build),
                        outcome: self.promote(p, now).await,
                    });
                }
            }
            Err(e) => items.push(Item {
                name: "pending units".to_string(),
                outcome: Reconciled::Failed(e.to_string()),
            }),
        }
        let after = self.cursor.lock().unwrap().clone();
        match self.maven.unversioned(after.as_ref(), self.limit).await {
            Ok(unversioned) => {
                *self.cursor.lock().unwrap() = if unversioned.len() < self.limit as usize {
                    None
                } else {
                    unversioned.last().cloned()
                };
                for u in &unversioned {
                    items.push(Item {
                        name: format!("version {}:{}", u.ga, u.version),
                        outcome: self.version(u, now).await,
                    });
                }
            }
            Err(e) => items.push(Item {
                name: "unversioned values".to_string(),
                outcome: Reconciled::Failed(e.to_string()),
            }),
        }
        items
    }
}
