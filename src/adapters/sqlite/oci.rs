//! `OciStore` over the `oci_blobs`, `oci_manifests`, `oci_manifest_blobs`,
//! `oci_tags` and `oci_uploads` tables.
//!
//! The two coarse methods are read-then-write, so both take the write lock up
//! front through [`immediate`]: a push replaces the manifest's link rows
//! before inserting them again, a delete counts the references it just
//! dropped.

use async_trait::async_trait;
use sqlx::SqlitePool;

use super::{immediate, store_error, Tx};
use crate::error::StoreError;
use crate::ports::oci::{Blob, Manifest, NewBlob, NewManifest, OciStore, Orphaned};

pub struct SqliteOciStore {
    pool: SqlitePool,
}

impl SqliteOciStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// The manifest row, then its links, then the tag that names it.
async fn write_manifest(tx: &mut Tx, m: &NewManifest<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO oci_manifests (repository_id, name, digest, content_type, size)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(m.repository)
    .bind(m.name)
    .bind(m.digest)
    .bind(m.content_type)
    .bind(m.size)
    .execute(&mut **tx)
    .await?;

    sqlx::query("DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2")
        .bind(m.repository)
        .bind(m.digest)
        .execute(&mut **tx)
        .await?;
    for blob in m.blobs {
        sqlx::query(
            "INSERT OR IGNORE INTO oci_manifest_blobs (repository_id, manifest_digest, blob_digest)
             VALUES (?1, ?2, ?3)",
        )
        .bind(m.repository)
        .bind(m.digest)
        .bind(blob)
        .execute(&mut **tx)
        .await?;
    }

    if let Some(tag) = m.tag {
        sqlx::query(
            "INSERT INTO oci_tags (repository_id, name, tag, manifest_digest)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repository_id, name, tag) DO UPDATE SET manifest_digest = excluded.manifest_digest",
        )
        .bind(m.repository)
        .bind(m.name)
        .bind(tag)
        .bind(m.digest)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The manifest, its tags and its links — and then, of the blobs those links
/// named, the ones nothing references any more.
async fn purge_manifest(
    tx: &mut Tx,
    repository: i64,
    name: &str,
    digest: &str,
) -> Result<Result<Option<Orphaned>, StoreError>, sqlx::Error> {
    let gone = sqlx::query(
        "DELETE FROM oci_manifests WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
    )
    .bind(repository)
    .bind(name)
    .bind(digest)
    .execute(&mut **tx)
    .await?;
    if gone.rows_affected() == 0 {
        return Ok(Ok(None));
    }
    sqlx::query(
        "DELETE FROM oci_tags WHERE repository_id = ?1 AND name = ?2 AND manifest_digest = ?3",
    )
    .bind(repository)
    .bind(name)
    .bind(digest)
    .execute(&mut **tx)
    .await?;

    let linked: Vec<String> = sqlx::query_scalar(
        "SELECT blob_digest FROM oci_manifest_blobs
         WHERE repository_id = ?1 AND manifest_digest = ?2",
    )
    .bind(repository)
    .bind(digest)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM oci_manifest_blobs WHERE repository_id = ?1 AND manifest_digest = ?2")
        .bind(repository)
        .bind(digest)
        .execute(&mut **tx)
        .await?;

    let mut blob_digests = Vec::new();
    for blob in linked {
        let still: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oci_manifest_blobs WHERE repository_id = ?1 AND blob_digest = ?2",
        )
        .bind(repository)
        .bind(&blob)
        .fetch_one(&mut **tx)
        .await?;
        if still == 0 {
            sqlx::query("DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2")
                .bind(repository)
                .bind(&blob)
                .execute(&mut **tx)
                .await?;
            blob_digests.push(blob);
        }
    }
    Ok(Ok(Some(Orphaned { blob_digests })))
}

#[async_trait]
impl OciStore for SqliteOciStore {
    async fn blob(&self, repository: i64, digest: &str) -> Result<Option<Blob>, StoreError> {
        let row: Option<(i64, Option<String>)> = sqlx::query_as(
            "SELECT size, content_type FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2",
        )
        .bind(repository)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(|(size, content_type)| Blob { size, content_type }))
    }

    async fn manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
    ) -> Result<Option<Manifest>, StoreError> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT content_type, size FROM oci_manifests
             WHERE repository_id = ?1 AND name = ?2 AND digest = ?3",
        )
        .bind(repository)
        .bind(name)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(|(content_type, size)| Manifest { content_type, size }))
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

    async fn put_manifest(&self, manifest: NewManifest<'_>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            let manifest = &manifest;
            Box::pin(async move {
                let landed = write_manifest(&mut tx, manifest).await.map(Ok);
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
    ) -> Result<Option<Orphaned>, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let gone = purge_manifest(&mut tx, repository, name, digest).await;
                (tx, gone)
            })
        })
        .await
    }

    async fn start_upload(&self, id: &str, repository: i64, name: &str) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO oci_uploads (id, repository_id, name) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(repository)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn upload_owner(&self, id: &str) -> Result<Option<i64>, StoreError> {
        sqlx::query_scalar("SELECT repository_id FROM oci_uploads WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)
    }

    async fn complete_upload(&self, id: &str, blob: NewBlob<'_>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            let blob = &blob;
            Box::pin(async move {
                let landed = close_upload(&mut tx, id, blob).await.map(Ok);
                (tx, landed)
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

    async fn delete_blob(&self, repository: i64, digest: &str) -> Result<bool, StoreError> {
        let gone = sqlx::query("DELETE FROM oci_blobs WHERE repository_id = ?1 AND digest = ?2")
            .bind(repository)
            .bind(digest)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(gone.rows_affected() > 0)
    }
}

/// The blob row and the ledger entry it came from.
async fn close_upload(tx: &mut Tx, id: &str, blob: &NewBlob<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO oci_blobs (repository_id, digest, size, content_type)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(blob.repository)
    .bind(blob.digest)
    .bind(blob.size)
    .bind(blob.content_type)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM oci_uploads WHERE id = ?1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
