//! Data-access layer for the OCI registry tables. Holds the row types that were
//! previously defined inside the OCI handlers (and, incrementally, the typed
//! queries that replace the inline SQL there).

#[derive(Debug, sqlx::FromRow)]
pub struct OciBlob {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub repository_id: i64,
    #[allow(dead_code)]
    pub digest: String,
    pub size: i64,
    pub content_type: Option<String>,
    #[allow(dead_code)]
    pub created_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct OciManifest {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub repository_id: i64,
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub digest: String,
    pub content_type: String,
    pub size: i64,
    #[allow(dead_code)]
    pub created_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct OciTag {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub repository_id: i64,
    #[allow(dead_code)]
    pub name: String,
    pub tag: String,
    pub manifest_digest: String,
    #[allow(dead_code)]
    pub created_at: String,
}

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
