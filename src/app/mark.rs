//! The high-water mark (ha-profiles C-3): the one guard that sees a
//! database put back in time by a path no door of ours watched — a file
//! copied over, an initContainer restore, a backup adopted by hand.
//!
//! The mark lives in the artifact store under the reserved prefix, carries
//! the installation, the epoch and a counter the reclamation writes
//! advance, and is compared both ways: a database behind the store draws a
//! fresh epoch, a store behind the database only owes a verify. Either way
//! reclamation is refused until that verify ends.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::domain::layout;
use crate::error::StoreError;
use crate::ports::reclaim::{Epoch, ReclaimStore};
use crate::storage::{StorageBackend, StorageError};

/// What the store holds about the database that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub installation: String,
    pub epoch: String,
    pub counter: u64,
}

/// How the database stands against the mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Level,
    /// The store carries reclamation this database never did.
    DatabaseBehind,
    /// The mark is gone or older than what this database wrote.
    StorageBehind,
}

#[derive(Debug, thiserror::Error)]
pub enum MarkError {
    #[error(
        "the artifact store carries the high-water mark of another installation ({0}): \
         two deployments must never share one store"
    )]
    Foreign(String),

    #[error("the high-water mark is unreadable")]
    Unreadable,

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub struct HighWaterMark {
    store: Arc<dyn ReclaimStore>,
    storage: Arc<dyn StorageBackend>,
}

impl HighWaterMark {
    pub fn new(store: Arc<dyn ReclaimStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { store, storage }
    }

    pub async fn read(&self) -> Result<Option<Mark>, MarkError> {
        match self.storage.get(layout::MARK).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| MarkError::Unreadable),
            Err(StorageError::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Read-only: what the two sides say of each other.
    pub async fn compare(&self, state: &Epoch) -> Result<Standing, MarkError> {
        let Some(mark) = self.read().await? else {
            return Ok(if state.counter == 0 {
                Standing::Level
            } else {
                Standing::StorageBehind
            });
        };
        if mark.installation != state.installation {
            return Err(MarkError::Foreign(mark.installation));
        }
        // An opaque epoch cannot be ordered: a mark under another epoch is
        // read as the database being behind, the conservative side.
        Ok(if mark.epoch != state.epoch || mark.counter > state.counter {
            Standing::DatabaseBehind
        } else if mark.counter < state.counter {
            Standing::StorageBehind
        } else {
            Standing::Level
        })
    }

    /// The comparison with its consequences: a fresh epoch for a database
    /// behind, a verify owed in both directions.
    pub async fn guard(&self) -> Result<Standing, MarkError> {
        let state = self.store.epoch().await?;
        let standing = self.compare(&state).await?;
        match standing {
            Standing::DatabaseBehind => {
                // The store's counter is the one that saw the deletions, so
                // the fresh epoch starts from it; the mark then carries that
                // epoch, or every pass would read the same lag again.
                let seen = self.read().await?.map_or(0, |mark| mark.counter);
                let drawn = self.store.new_epoch(seen).await?;
                self.write(&drawn).await?;
            }
            Standing::StorageBehind => {
                self.store.require_verify().await?;
                self.write(&self.store.epoch().await?).await?;
            }
            Standing::Level => {}
        }
        Ok(standing)
    }

    /// The mark as the database now stands.
    async fn write(&self, state: &Epoch) -> Result<(), MarkError> {
        let mark = Mark {
            installation: state.installation.clone(),
            epoch: state.epoch.clone(),
            counter: state.counter,
        };
        let body = serde_json::to_vec(&mark).map_err(|_| MarkError::Unreadable)?;
        self.storage.put(layout::MARK, body.into()).await?;
        Ok(())
    }

    /// The right to delete: the mark is written one ahead first, then the
    /// database records it. A crash between the two leaves the store ahead,
    /// which the next comparison reads as the conservative case.
    pub async fn reserve(&self, state: &Epoch) -> Result<bool, MarkError> {
        if state.verify_pending {
            return Ok(false);
        }
        let next = state.counter + 1;
        let mark = Mark {
            installation: state.installation.clone(),
            epoch: state.epoch.clone(),
            counter: next,
        };
        let body = serde_json::to_vec(&mark).map_err(|_| MarkError::Unreadable)?;
        self.storage.put(layout::MARK, body.into()).await?;
        Ok(self.store.advance(&state.epoch, next).await?)
    }

    /// Whether a batch may delete now: level with the store, no verify
    /// owed, and the mark written ahead of the counter it advances.
    pub async fn permit(&self) -> Result<bool, MarkError> {
        if self.guard().await? != Standing::Level {
            return Ok(false);
        }
        let state = self.store.epoch().await?;
        self.reserve(&state).await
    }
}

#[cfg(test)]
#[path = "mark_tests.rs"]
mod tests;
