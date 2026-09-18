//! `place_shared`: the only placer of shared keys (A1 C5bis).
//!
//! Pin every entry in one batch, write each entry's bytes to its fresh (or
//! reused) generation, then hand the tokens to the use case's commit, which
//! spends them by compare-and-set in its own transaction. A revoked pin makes
//! the commit write nothing; only the revoked entries are pinned and placed
//! again. Whatever this call wrote and no row came to reference is
//! enqueued, never deleted.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::domain::layout;
use crate::error::{AppError, StoreError};
use crate::ports::reclaim::{PinToken, Pinned, ReclaimStore};
use crate::storage::{StorageBackend, StorageError};

/// Where an entry's bytes come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Held in memory: re-placeable as often as needed.
    Bytes(Bytes),
    /// A committed object that stays referenced while the call runs.
    Copy(String),
    /// A private object this call wrote; moved into place, not copied.
    Draft(String),
    /// Private objects that stay readable while the call runs, streamed one
    /// after the other. A generation already holding `size` bytes is a
    /// reused one, whose content the key names: it is not written again.
    Segments { keys: Vec<String>, size: u64 },
    /// A key a committed row already holds: pinned so the commit is fenced,
    /// never written.
    Existing,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub logical_key: String,
    pub source: Source,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    /// The incarnation was retired: nothing was written.
    #[error("the repository was removed")]
    Retired,

    /// The commit refused; every generation of this attempt is enqueued.
    #[error(transparent)]
    Refused(StoreError),

    /// A revoked entry could not be placed again: retryable, nothing
    /// recorded.
    #[error("the placement was superseded and could not be replayed, try again")]
    Unavailable,

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<PlaceError> for AppError {
    fn from(err: PlaceError) -> Self {
        match err {
            PlaceError::Retired => AppError::NotFound("the repository was removed".to_string()),
            PlaceError::Refused(err) | PlaceError::Store(err) => err.into(),
            PlaceError::Unavailable => AppError::ServiceUnavailable(err.to_string()),
            PlaceError::Storage(err) => err.into(),
        }
    }
}

const ATTEMPTS: usize = 3;

pub struct Placer {
    reclaim: Arc<dyn ReclaimStore>,
    storage: Arc<dyn StorageBackend>,
}

impl Placer {
    pub fn new(reclaim: Arc<dyn ReclaimStore>, storage: Arc<dyn StorageBackend>) -> Self {
        Self { reclaim, storage }
    }

    /// Liveness only: how long a dead placer's pins block a claim.
    fn pin_until(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let bound = self.storage.upload_plan().completion_bound * 2 + Duration::from_secs(60);
        now + chrono::Duration::from_std(bound).unwrap_or(chrono::Duration::hours(1))
    }

    async fn pin(
        &self,
        repo_prefix: &str,
        logical: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<PinToken>, PlaceError> {
        match self
            .reclaim
            .pin(repo_prefix, logical, self.pin_until(now))
            .await?
        {
            Pinned::Tokens(tokens) => Ok(tokens),
            Pinned::Retired => Err(PlaceError::Retired),
        }
    }

    /// Whether this call wrote `to`: a skipped or existing entry is not its
    /// residue.
    async fn write(&self, source: &Source, to: &str) -> Result<bool, StorageError> {
        match source {
            Source::Bytes(bytes) => self.storage.put(to, bytes.clone()).await.map(|()| true),
            Source::Copy(from) => self.storage.copy_object(from, to).await.map(|()| true),
            Source::Draft(draft) => self.storage.relocate(draft, to).await.map(|()| true),
            Source::Segments { keys, size } => {
                if self.storage.stat(to).await?.is_some_and(|m| m.size == *size) {
                    return Ok(false);
                }
                self.concat(keys, to).await.map(|()| true)
            }
            Source::Existing => Ok(false),
        }
    }

    /// What a draft's generation committed, the size a replay checks.
    async fn size_of(&self, source: &Source, key: &str) -> Option<u64> {
        match source {
            Source::Draft(_) => self.storage.stat(key).await.ok().flatten().map(|m| m.size),
            _ => None,
        }
    }

    async fn concat(&self, keys: &[String], to: &str) -> Result<(), StorageError> {
        use tokio::io::AsyncReadExt;
        let mut writer = self.storage.writer(to).await?;
        let mut buf = vec![0u8; 1 << 20];
        for key in keys {
            let mut body = self.storage.read_stream(key).await?.body;
            loop {
                let n = body.read(&mut buf).await.map_err(|_| StorageError::Unavailable)?;
                if n == 0 {
                    break;
                }
                writer.reserve(n).await?;
                writer.write(Bytes::copy_from_slice(&buf[..n])).await?;
            }
        }
        writer.commit().await?;
        Ok(())
    }

    /// T3: a source that survives is read again; a draft that was moved is
    /// copied back from the revoked generation only when `stat` shows it
    /// whole; otherwise (`None`) the entry cannot be replayed.
    async fn rewrite(
        &self,
        source: &Source,
        revoked: &str,
        size: Option<u64>,
        to: &str,
    ) -> Result<Option<bool>, StorageError> {
        match source {
            Source::Bytes(_) | Source::Copy(_) | Source::Segments { .. } | Source::Existing => {
                self.write(source, to).await.map(Some)
            }
            Source::Draft(_) => {
                let whole = match (self.storage.stat(revoked).await?, size) {
                    (Some(meta), Some(size)) => meta.size == size,
                    _ => false,
                };
                if !whole {
                    return Ok(None);
                }
                match self.storage.copy_object(revoked, to).await {
                    Ok(()) => Ok(Some(true)),
                    Err(StorageError::NotFound) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }
    }

    async fn enqueue(&self, keys: &[String], now: DateTime<Utc>) {
        if keys.is_empty() {
            return;
        }
        if let Err(e) = self.reclaim.enqueue(keys, now).await {
            tracing::warn!(error = %e, ?keys, "placement residue not enqueued; the scan will find it");
        }
    }

    /// Places `entries` under `repo_prefix` and runs `commit` with one token
    /// per entry, in entry order.
    pub async fn place_shared<T, F, Fut>(
        &self,
        repo_prefix: &str,
        entries: &[Entry],
        mut commit: F,
        now: DateTime<Utc>,
    ) -> Result<T, PlaceError>
    where
        F: FnMut(Vec<PinToken>) -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        let logical: Vec<String> = entries.iter().map(|e| e.logical_key.clone()).collect();
        let mut tokens = self.pin(repo_prefix, &logical, now).await?;
        let mut written: Vec<String> = Vec::new();
        let mut sizes: Vec<Option<u64>> = vec![None; entries.len()];
        for (i, (entry, token)) in entries.iter().zip(&tokens).enumerate() {
            match self.write(&entry.source, &token.physical_key).await {
                Ok(true) => written.push(token.physical_key.clone()),
                Ok(false) => {}
                Err(e) => {
                    self.enqueue(&written, now).await;
                    return Err(e.into());
                }
            }
            sizes[i] = self.size_of(&entry.source, &token.physical_key).await;
        }

        for attempt in 0..ATTEMPTS {
            match commit(tokens.clone()).await {
                Ok(done) => return Ok(done),
                Err(StoreError::Superseded(revoked)) if attempt + 1 < ATTEMPTS => {
                    let again: Vec<usize> = (0..entries.len())
                        .filter(|&i| revoked.contains(&tokens[i].physical_key))
                        .collect();
                    let logical: Vec<String> =
                        again.iter().map(|&i| entries[i].logical_key.clone()).collect();
                    let fresh = match self.pin(repo_prefix, &logical, now).await {
                        Ok(fresh) => fresh,
                        Err(e) => {
                            self.enqueue(&unreferenced(&written, &revoked), now).await;
                            return Err(e);
                        }
                    };
                    for (&i, token) in again.iter().zip(fresh) {
                        let replayed = self
                            .rewrite(&entries[i].source, &tokens[i].physical_key, sizes[i], &token.physical_key)
                            .await;
                        match replayed {
                            Ok(Some(wrote)) => {
                                if wrote {
                                    written.push(token.physical_key.clone());
                                }
                                sizes[i] = self.size_of(&entries[i].source, &token.physical_key).await;
                                tokens[i] = token;
                            }
                            Ok(None) => {
                                let mut residue = unreferenced(&written, &revoked);
                                residue.push(token.physical_key.clone());
                                self.enqueue(&residue, now).await;
                                return Err(PlaceError::Unavailable);
                            }
                            Err(e) => {
                                self.enqueue(&unreferenced(&written, &revoked), now).await;
                                return Err(e.into());
                            }
                        }
                    }
                }
                Err(StoreError::Superseded(revoked)) => {
                    self.enqueue(&unreferenced(&written, &revoked), now).await;
                    return Err(PlaceError::Unavailable);
                }
                Err(refused) => {
                    self.enqueue(&written, now).await;
                    return Err(PlaceError::Refused(refused));
                }
            }
        }
        unreachable!("the last attempt always returns")
    }

    /// A private draft for a body whose digest is unknown until it ends:
    /// pinned like any placement, under the incarnation's draft segment.
    pub async fn draft(&self, repo_prefix: &str, now: DateTime<Utc>) -> Result<String, PlaceError> {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let logical = vec![layout::draft_key(repo_prefix, &nonce)];
        let tokens = self.pin(repo_prefix, &logical, now).await?;
        Ok(tokens[0].physical_key.clone())
    }

    /// Its one writer deletes a draft a relocation left behind.
    pub async fn drop_draft(&self, draft: &str) {
        let _ = self.storage.delete(draft).await;
    }
}

/// Every generation this call wrote, less the revoked ones: those belong to
/// the claim that revoked them.
fn unreferenced(written: &[String], revoked: &[String]) -> Vec<String> {
    written
        .iter()
        .filter(|k| !revoked.contains(k))
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "place_tests.rs"]
mod tests;
