//! What is left of the OCI data-access layer: the upload ledger, which the
//! push half still reads through the pool. The three read queries a leaf
//! makes are `OciStore`'s now.

use sqlx::SqlitePool;

#[derive(Debug, sqlx::FromRow)]
pub struct OciUpload {
    #[allow(dead_code)]
    pub id: String,
    pub repository_id: i64,
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub started_at: String,
}

pub async fn get_upload(
    pool: &SqlitePool,
    upload_id: &str,
) -> Result<Option<OciUpload>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM oci_uploads WHERE id = ?1")
        .bind(upload_id)
        .fetch_optional(pool)
        .await
}
