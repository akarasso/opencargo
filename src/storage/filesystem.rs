use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::fs;

use crate::error::AppError;

use super::StorageBackend;

pub struct FilesystemStorage {
    base_path: PathBuf,
}

impl FilesystemStorage {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        let base_path = base_path.into();
        std::fs::create_dir_all(&base_path).expect("failed to create storage base directory");
        let base_path = base_path
            .canonicalize()
            .expect("failed to canonicalize storage base directory");
        Self { base_path }
    }

    /// Resolve a relative path and ensure it stays within the base directory.
    /// Prevents path traversal attacks (e.g., "../../etc/passwd").
    fn safe_path(&self, path: &str) -> Result<PathBuf, AppError> {
        // Reject obvious traversal attempts before touching the filesystem
        if path.contains("..") {
            return Err(AppError::BadRequest(
                "path must not contain '..'".to_string(),
            ));
        }
        let full_path = self.base_path.join(path);
        // For existing files, canonicalize and verify prefix
        if full_path.exists() {
            let canonical = full_path.canonicalize().map_err(|_| {
                AppError::BadRequest("invalid storage path".to_string())
            })?;
            if !canonical.starts_with(&self.base_path) {
                return Err(AppError::BadRequest(
                    "path escapes storage directory".to_string(),
                ));
            }
            return Ok(canonical);
        }
        // For new files, verify that the joined path stays under base
        // by checking the normalized components
        let normalized = full_path
            .components()
            .fold(PathBuf::new(), |mut acc, comp| {
                match comp {
                    std::path::Component::ParentDir => { acc.pop(); }
                    other => acc.push(other),
                }
                acc
            });
        if !normalized.starts_with(&self.base_path) {
            return Err(AppError::BadRequest(
                "path escapes storage directory".to_string(),
            ));
        }
        Ok(full_path)
    }
}

#[async_trait]
impl StorageBackend for FilesystemStorage {
    async fn get(&self, path: &str) -> Result<Bytes, AppError> {
        let full_path = self.safe_path(path)?;
        if !full_path.exists() {
            return Err(AppError::NotFound(format!("file not found: {path}")));
        }
        let data = fs::read(&full_path).await?;
        Ok(Bytes::from(data))
    }

    async fn put(&self, path: &str, data: Bytes) -> Result<(), AppError> {
        let full_path = self.safe_path(path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&full_path, &data).await?;
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<(), AppError> {
        let full_path = self.safe_path(path)?;
        if full_path.exists() {
            fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool, AppError> {
        let full_path = self.safe_path(path)?;
        Ok(full_path.exists())
    }
}
