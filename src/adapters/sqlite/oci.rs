//! `OciStore` over the `oci_blobs`, `oci_manifests` and `oci_tags` tables.

use async_trait::async_trait;
use sqlx::SqlitePool;

use super::store_error;
use crate::error::StoreError;
use crate::ports::oci::{Blob, Manifest, OciStore};

pub struct SqliteOciStore {
    pool: SqlitePool,
}

impl SqliteOciStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
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
        sqlx::query_scalar("SELECT tag FROM oci_tags WHERE repository_id = ?1 AND name = ?2 ORDER BY tag")
            .bind(repository)
            .bind(name)
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)
    }
}
