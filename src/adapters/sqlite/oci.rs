//! `OciStore` over the `oci_blobs`, `oci_manifests`, `oci_manifest_blobs`,
//! `oci_tags`, `oci_uploads` and `oci_upload_segments` tables.
//!
//! Every coarse method is read-then-write and runs under [`immediate`]. The
//! commits spend their pins with [`reclaim::spend_pins`] and the deletes
//! enqueue with [`reclaim::enqueue_keys`], in their own transaction.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, immediate, reclaim, store_error, Tx};
use crate::error::StoreError;
use crate::ports::oci::{
    BeginComplete, Blob, Finished, LeaseToken, Manifest, NewBlob, NewManifest, OciStore, Orphaned,
    Segment, SegmentClaim, UploadSession,
};
use crate::ports::reclaim::PinToken;

pub struct SqliteOciStore {
    pool: SqlitePool,
}

impl SqliteOciStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

type Step<T> = Result<Result<T, StoreError>, sqlx::Error>;

fn after(now: DateTime<Utc>, ttl: Duration) -> DateTime<Utc> {
    now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX)
}

fn before(now: DateTime<Utc>, age: Duration) -> DateTime<Utc> {
    now - chrono::Duration::from_std(age).unwrap_or(chrono::Duration::MAX)
}

/// The pins, the blob rows, then the manifest row, its links and its tag.
/// What a repository holds of this format: every OCI row whose key to the
/// repository does not cascade, so the emptiness probe answers before the
/// delete trips it.
pub(crate) async fn rows(tx: &mut Tx, repository: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM oci_manifests WHERE repository_id = ?1)
              + (SELECT COUNT(*) FROM oci_blobs WHERE repository_id = ?1)
              + (SELECT COUNT(*) FROM oci_tags WHERE repository_id = ?1)
              + (SELECT COUNT(*) FROM oci_uploads WHERE repository_id = ?1)",
    )
    .bind(repository)
    .fetch_one(&mut **tx)
    .await
}

async fn write_manifest(tx: &mut Tx, m: &NewManifest<'_>, now: DateTime<Utc>) -> Step<()> {
    let mut pins: Vec<PinToken> = vec![m.pin.clone()];
    pins.extend(m.blobs.iter().map(|(_, pin)| pin.clone()));
    if let Err(revoked) = reclaim::spend_pins(tx, &pins).await? {
        return Ok(Err(StoreError::Superseded(revoked)));
    }
    for (digest, _) in m.blobs {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2)",
        )
        .bind(m.repository)
        .bind(digest)
        .fetch_one(&mut **tx)
        .await?;
        if !known {
            return Ok(Err(StoreError::NotFound));
        }
    }

    let previous: Option<String> = sqlx::query_scalar(
        "SELECT storage_key FROM oci_manifests
         WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
    )
    .bind(m.repository)
    .bind(m.name)
    .bind(m.digest)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    if let Some(previous) = previous.filter(|k| *k != m.pin.physical_key) {
        reclaim::enqueue_keys(tx, &[previous], now).await?;
    }
    sqlx::query(
        "INSERT INTO oci_manifests (repository_id, name, digest, content_type, size, storage_key, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(repository_id, name, digest) DO UPDATE SET
             content_type = excluded.content_type,
             size = excluded.size,
             storage_key = excluded.storage_key",
    )
    .bind(m.repository)
    .bind(m.name)
    .bind(m.digest)
    .bind(m.content_type)
    .bind(m.size)
    .bind(&m.pin.physical_key)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;

    sqlx::query("DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2")
        .bind(m.repository)
        .bind(m.digest)
        .execute(&mut **tx)
        .await?;
    let linked = m.blobs.iter().map(|(d, _)| d).chain(m.children.iter());
    for blob in linked {
        sqlx::query(
            "INSERT INTO oci_manifest_blobs (repository_id, manifest_digest, blob_digest)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(repository_id, manifest_digest, blob_digest) DO NOTHING",
        )
        .bind(m.repository)
        .bind(m.digest)
        .bind(blob)
        .execute(&mut **tx)
        .await?;
    }

    if let Some(tag) = m.tag {
        sqlx::query(
            "INSERT INTO oci_tags (repository_id, name, tag, manifest_digest, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repository_id, name, tag) DO UPDATE SET manifest_digest = excluded.manifest_digest",
        )
        .bind(m.repository)
        .bind(m.name)
        .bind(tag)
        .bind(m.digest)
        .bind(bind_ts(now))
        .execute(&mut **tx)
        .await?;
    }
    Ok(Ok(()))
}

/// The manifest, its tags and its links, then the blobs nothing lists any
/// more; every released key is enqueued.
async fn purge_manifest(
    tx: &mut Tx,
    repository: i64,
    name: &str,
    digest: &str,
    now: DateTime<Utc>,
) -> Step<Option<Orphaned>> {
    let key: Option<Option<String>> = sqlx::query_scalar(
        "SELECT storage_key FROM oci_manifests
         WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
    )
    .bind(repository)
    .bind(name)
    .bind(digest)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(key) = key else {
        return Ok(Ok(None));
    };
    sqlx::query("DELETE FROM oci_manifests WHERE repository_id = ?1 AND name = ?2 AND digest = ?3")
        .bind(repository)
        .bind(name)
        .bind(digest)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "DELETE FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND manifest_digest = ?3",
    )
    .bind(repository)
    .bind(name)
    .bind(digest)
    .execute(&mut **tx)
    .await?;

    let still_listed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oci_manifests WHERE repository_id = ?1 AND digest = ?2)",
    )
    .bind(repository)
    .bind(digest)
    .fetch_one(&mut **tx)
    .await?;
    let mut released: Vec<String> = key.into_iter().collect();
    let mut blob_digests = Vec::new();
    if !still_listed {
        let linked: Vec<String> = sqlx::query_scalar(
            "SELECT blob_digest FROM oci_manifest_blobs
             WHERE repository_id = ?1 AND manifest_digest = ?2",
        )
        .bind(repository)
        .bind(digest)
        .fetch_all(&mut **tx)
        .await?;
        sqlx::query(
            "DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2",
        )
        .bind(repository)
        .bind(digest)
        .execute(&mut **tx)
        .await?;
        for blob in linked {
            let still: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM oci_manifest_blobs
                                WHERE repository_id = ?1 AND blob_digest = ?2)",
            )
            .bind(repository)
            .bind(&blob)
            .fetch_one(&mut **tx)
            .await?;
            if still {
                continue;
            }
            let gone: Option<Option<String>> = sqlx::query_scalar(
                "DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2
                 RETURNING storage_key",
            )
            .bind(repository)
            .bind(&blob)
            .fetch_optional(&mut **tx)
            .await?;
            if let Some(key) = gone {
                released.extend(key);
                blob_digests.push(blob);
            }
        }
    }
    reclaim::enqueue_keys(tx, &released, now).await?;
    Ok(Ok(Some(Orphaned { blob_digests })))
}

async fn drop_blob(tx: &mut Tx, repository: i64, digest: &str, now: DateTime<Utc>) -> Step<bool> {
    let listed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2)",
    )
    .bind(repository)
    .bind(digest)
    .fetch_one(&mut **tx)
    .await?;
    if listed {
        return Ok(Err(StoreError::Conflict));
    }
    let gone: Option<Option<String>> = sqlx::query_scalar(
        "DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2 RETURNING storage_key",
    )
    .bind(repository)
    .bind(digest)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(key) = gone else {
        return Ok(Ok(false));
    };
    reclaim::enqueue_keys(tx, &key.into_iter().collect::<Vec<_>>(), now).await?;
    Ok(Ok(true))
}

async fn claim_one_segment(
    tx: &mut Tx,
    id: &str,
    segment: &Segment,
    max_segments: u32,
    now: DateTime<Utc>,
) -> Step<SegmentClaim> {
    let row: Option<(i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT received, segment_count, lease_until FROM oci_uploads
         WHERE id = ?1 AND received IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((received, count, lease_until)) = row else {
        return Ok(Ok(SegmentClaim::Lost));
    };
    let completing = lease_until.is_some_and(|until| until > bind_ts(now));
    if completing || received as u64 != segment.start {
        return Ok(Ok(SegmentClaim::Lost));
    }
    if count as u64 >= u64::from(max_segments) {
        return Ok(Ok(SegmentClaim::TooManySegments));
    }
    sqlx::query(
        "INSERT INTO oci_upload_segments (upload_id, start_at, length, storage_key)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(id)
    .bind(segment.start as i64)
    .bind(segment.len as i64)
    .bind(&segment.key)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE oci_uploads SET received = received + ?2, segment_count = segment_count + 1,
             touched_at = ?3
         WHERE id = ?1",
    )
    .bind(id)
    .bind(segment.len as i64)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    Ok(Ok(SegmentClaim::Won))
}

async fn lease(tx: &mut Tx, id: &str, now: DateTime<Utc>, ttl: Duration) -> Step<BeginComplete> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT lease_until FROM oci_uploads WHERE id = ?1 AND received IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((until,)) = row else {
        return Ok(Ok(BeginComplete::Unknown));
    };
    if until.is_some_and(|until| until > bind_ts(now)) {
        return Ok(Ok(BeginComplete::Held));
    }
    let token = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "UPDATE oci_uploads SET lease_token = ?2, lease_until = ?3, touched_at = ?4 WHERE id = ?1",
    )
    .bind(id)
    .bind(&token)
    .bind(bind_ts(after(now, ttl)))
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    Ok(Ok(BeginComplete::Lease(LeaseToken(token))))
}

async fn finish(
    tx: &mut Tx,
    id: &str,
    lease: &LeaseToken,
    pin: &PinToken,
    blob: &NewBlob<'_>,
    now: DateTime<Utc>,
) -> Step<Finished> {
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oci_uploads WHERE id = ?1 AND lease_token = ?2)",
    )
    .bind(id)
    .bind(&lease.0)
    .fetch_one(&mut **tx)
    .await?;
    if !held {
        return Ok(Ok(Finished::LeaseLost));
    }
    if let Err(revoked) = reclaim::spend_pins(tx, std::slice::from_ref(pin)).await? {
        return Ok(Err(StoreError::Superseded(revoked)));
    }
    let existing: Option<Option<String>> = sqlx::query_scalar(
        "SELECT storage_key FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2",
    )
    .bind(blob.repository)
    .bind(blob.digest)
    .fetch_optional(&mut **tx)
    .await?;
    let recorded = match existing {
        Some(Some(key)) => {
            if key != pin.physical_key {
                reclaim::enqueue_keys(tx, std::slice::from_ref(&pin.physical_key), now).await?;
            }
            key
        }
        _ => {
            sqlx::query(
                "INSERT INTO oci_blobs (repository_id, digest, size, content_type, storage_key, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(repository_id, digest) DO UPDATE SET storage_key = excluded.storage_key",
            )
            .bind(blob.repository)
            .bind(blob.digest)
            .bind(blob.size)
            .bind(blob.content_type)
            .bind(&pin.physical_key)
            .bind(bind_ts(now))
            .execute(&mut **tx)
            .await?;
            pin.physical_key.clone()
        }
    };
    sqlx::query("DELETE FROM oci_upload_segments WHERE upload_id = ?1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM oci_uploads WHERE id = ?1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(Ok(Finished::Recorded(recorded)))
}

async fn reap(tx: &mut Tx, idle: Duration, now: DateTime<Utc>, limit: u32) -> Step<u64> {
    let dead: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, COALESCE(segment_prefix, 'oci/_uploads/' || id) FROM oci_uploads
         WHERE received IS NULL
            OR (touched_at <= ?1 AND (lease_until IS NULL OR lease_until <= ?2))
         ORDER BY id LIMIT ?3",
    )
    .bind(bind_ts(before(now, idle)))
    .bind(bind_ts(now))
    .bind(i64::from(limit))
    .fetch_all(&mut **tx)
    .await?;
    for (id, prefix) in &dead {
        sqlx::query("DELETE FROM oci_upload_segments WHERE upload_id = ?1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM oci_uploads WHERE id = ?1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        reclaim::enqueue_prefix(tx, prefix, now).await?;
    }
    Ok(Ok(dead.len() as u64))
}

#[async_trait]
impl OciStore for SqliteOciStore {
    async fn blob(&self, repository: i64, digest: &str) -> Result<Option<Blob>, StoreError> {
        let row: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT size, content_type, storage_key FROM oci_blobs
             WHERE repository_id = ?1 AND digest = ?2",
        )
        .bind(repository)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.and_then(|(size, content_type, key)| {
            Some(Blob {
                size,
                content_type,
                key: key?,
            })
        }))
    }

    async fn manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
    ) -> Result<Option<Manifest>, StoreError> {
        let row: Option<(String, i64, Option<String>)> = sqlx::query_as(
            "SELECT content_type, size, storage_key FROM oci_manifests
             WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
        )
        .bind(repository)
        .bind(name)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.and_then(|(content_type, size, key)| {
            Some(Manifest {
                content_type,
                size,
                key: key?,
            })
        }))
    }

    async fn digest_for_ref(
        &self,
        repository: i64,
        name: &str,
        reference: &str,
    ) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT manifest_digest FROM oci_tags
             WHERE repository_id = ?1 AND name = ?2 AND tag = ?3",
        )
        .bind(repository)
        .bind(name)
        .bind(reference)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn tags(&self, repository: i64, name: &str) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT tag FROM oci_tags WHERE repository_id = ?1 AND name = ?2 ORDER BY tag",
        )
        .bind(repository)
        .bind(name)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn put_manifest(
        &self,
        manifest: NewManifest<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            let manifest = &manifest;
            Box::pin(async move {
                let landed = write_manifest(&mut tx, manifest, now).await;
                (tx, landed)
            })
        })
        .await
    }

    async fn delete_manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Orphaned>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let gone = purge_manifest(&mut tx, repository, name, digest, now).await;
                (tx, gone)
            })
        })
        .await
    }

    async fn blob_references(&self, repository: i64, digest: &str) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2",
        )
        .bind(repository)
        .bind(digest)
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)
    }

    async fn delete_blob(
        &self,
        repository: i64,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let gone = drop_blob(&mut tx, repository, digest, now).await;
                (tx, gone)
            })
        })
        .await
    }

    async fn start_upload(
        &self,
        id: &str,
        repository: i64,
        name: &str,
        prefix: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO oci_uploads
                 (id, repository_id, name, started_at, segment_prefix, received, segment_count, touched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?4)",
        )
        .bind(id)
        .bind(repository)
        .bind(name)
        .bind(bind_ts(now))
        .bind(prefix)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn upload(&self, id: &str) -> Result<Option<UploadSession>, StoreError> {
        let row: Option<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT repository_id, segment_prefix, received, segment_count FROM oci_uploads
             WHERE id = ?1 AND received IS NOT NULL AND segment_prefix IS NOT NULL",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(|(repository, prefix, received, segments)| UploadSession {
            repository,
            prefix,
            received: received.max(0) as u64,
            segments: segments.clamp(0, i64::from(u32::MAX)) as u32,
        }))
    }

    async fn claim_segment(
        &self,
        id: &str,
        segment: &Segment,
        max_segments: u32,
        now: DateTime<Utc>,
    ) -> Result<SegmentClaim, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let won = claim_one_segment(&mut tx, id, segment, max_segments, now).await;
                (tx, won)
            })
        })
        .await
    }

    async fn segments(&self, id: &str) -> Result<Vec<Segment>, StoreError> {
        let rows: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT start_at, length, storage_key FROM oci_upload_segments
             WHERE upload_id = ?1 ORDER BY start_at",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(start, len, key)| Segment {
                start: start.max(0) as u64,
                len: len.max(0) as u64,
                key,
            })
            .collect())
    }

    async fn begin_complete(
        &self,
        id: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<BeginComplete, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let taken = lease(&mut tx, id, now, ttl).await;
                (tx, taken)
            })
        })
        .await
    }

    async fn release_complete(&self, id: &str, lease: &LeaseToken) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE oci_uploads SET lease_token = NULL, lease_until = NULL
             WHERE id = ?1 AND lease_token = ?2",
        )
        .bind(id)
        .bind(&lease.0)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn finish_upload(
        &self,
        id: &str,
        lease: &LeaseToken,
        pin: &PinToken,
        blob: NewBlob<'_>,
        now: DateTime<Utc>,
    ) -> Result<Finished, StoreError> {
        immediate(&self.pool, |mut tx| {
            let blob = &blob;
            Box::pin(async move {
                let done = finish(&mut tx, id, lease, pin, blob, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn reap_uploads(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let reaped = reap(&mut tx, idle, now, limit).await;
                (tx, reaped)
            })
        })
        .await
    }
}
