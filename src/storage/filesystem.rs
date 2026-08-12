use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::fs;
use tokio::io::AsyncWriteExt;

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
        // Component normalization alone does not see the filesystem: a symlink
        // already planted inside the base and pointing outside would let a
        // *new* file be created on the symlink's target side. Canonicalize the
        // deepest existing ancestor (walking up component by component) and
        // verify it still lives under the canonical base before re-appending
        // the not-yet-existing suffix (already normalized, no `..` possible).
        // `symlink_metadata` (not `exists`) so a dangling symlink counts as
        // the existing ancestor: its failing canonicalize is then rejected
        // instead of being silently walked past.
        let mut ancestor = normalized.as_path();
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        while ancestor.symlink_metadata().is_err() {
            match (ancestor.parent(), ancestor.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    ancestor = parent;
                }
                _ => {
                    return Err(AppError::BadRequest("invalid storage path".to_string()));
                }
            }
        }
        let canonical = ancestor
            .canonicalize()
            .map_err(|_| AppError::BadRequest("invalid storage path".to_string()))?;
        if !canonical.starts_with(&self.base_path) {
            return Err(AppError::BadRequest(
                "path escapes storage directory".to_string(),
            ));
        }
        let mut resolved = canonical;
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
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

    async fn append(&self, path: &str, data: Bytes) -> Result<u64, AppError> {
        let full_path = self.safe_path(path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&full_path)
            .await?;
        file.write_all(&data).await?;
        file.flush().await?;
        let len = file.metadata().await?.len();
        Ok(len)
    }

    async fn delete(&self, path: &str) -> Result<(), AppError> {
        let full_path = self.safe_path(path)?;
        if full_path.exists() {
            fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError> {
        let full_path = self.safe_path(prefix)?;
        if full_path.is_dir() {
            fs::remove_dir_all(&full_path).await?;
        } else if full_path.exists() {
            fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool, AppError> {
        let full_path = self.safe_path(path)?;
        Ok(full_path.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::FilesystemStorage;
    use crate::error::AppError;

    /// Storage rooted in a fresh temp dir. The `TempDir` guard must stay
    /// alive for the duration of the test (drop deletes the tree).
    fn storage() -> (tempfile::TempDir, FilesystemStorage) {
        let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
        let storage = FilesystemStorage::new(tmp.path().join("base"));
        (tmp, storage)
    }

    fn is_bad_request(res: &Result<std::path::PathBuf, AppError>) -> bool {
        matches!(res, Err(AppError::BadRequest(_)))
    }

    #[test]
    fn accepts_simple_and_nested_relative_paths() {
        let (_tmp, s) = storage();

        let p = s.safe_path("file.txt").expect("simple path must resolve");
        assert!(p.starts_with(&s.base_path));
        assert!(p.ends_with("file.txt"));

        let p = s
            .safe_path("npm/my-repo/pkg/pkg-1.0.0.tgz")
            .expect("nested path must resolve");
        assert!(p.starts_with(&s.base_path));
        assert!(p.ends_with("npm/my-repo/pkg/pkg-1.0.0.tgz"));
    }

    #[test]
    fn normalizes_paths_to_nonexistent_files() {
        let (_tmp, s) = storage();
        // Nothing under "a/" exists yet: this exercises the component-
        // normalization branch (no canonicalize possible).
        let p = s
            .safe_path("a/b/new-file.bin")
            .expect("path to a new file must resolve");
        assert!(p.starts_with(&s.base_path));
        // CurDir components are absorbed by Path::components().
        let p = s
            .safe_path("./a/./c.txt")
            .expect("CurDir components are harmless");
        assert!(p.starts_with(&s.base_path));
    }

    #[test]
    fn rejects_parent_dir_components() {
        let (_tmp, s) = storage();
        for attempt in [
            "..",
            "../escape.txt",
            "../../etc/passwd",
            "a/../../etc/passwd",
            "a/b/../../../etc/passwd",
            "a/..",
        ] {
            let res = s.safe_path(attempt);
            assert!(
                is_bad_request(&res),
                "traversal attempt {attempt:?} must be rejected, got {res:?}"
            );
        }
    }

    /// The guard is a plain substring check on "..": even a legitimate file
    /// name that merely *contains* two consecutive dots is rejected. Overly
    /// strict, but fail-safe — documented here as current behavior.
    #[test]
    fn rejects_double_dots_anywhere_in_a_component() {
        let (_tmp, s) = storage();
        for attempt in ["foo..bar.txt", "x/fo..o/y.txt", "pkg-1.0..tgz"] {
            let res = s.safe_path(attempt);
            assert!(
                is_bad_request(&res),
                "{attempt:?} contains '..' as a substring and is rejected, got {res:?}"
            );
        }
    }

    /// Percent-encoding is not decoded at this layer (the HTTP layer decodes
    /// before storage paths are built): "%2e%2e" is a literal directory name
    /// here, so it stays safely inside the base.
    #[test]
    fn percent_encoded_dotdot_is_treated_as_a_literal_name() {
        let (_tmp, s) = storage();
        let p = s
            .safe_path("%2e%2e/escape.txt")
            .expect("literal %2e%2e is just an odd directory name");
        assert!(p.starts_with(&s.base_path));
    }

    #[test]
    fn rejects_absolute_paths() {
        let (_tmp, s) = storage();
        // Path::join replaces the base entirely when handed an absolute path.
        // Existing target → canonicalize branch catches the escape.
        let res = s.safe_path("/etc/passwd");
        assert!(
            is_bad_request(&res),
            "absolute path to an existing file must be rejected, got {res:?}"
        );
        // Nonexistent target → normalization branch catches it too.
        let res = s.safe_path("/definitely/not/existing/xyz.txt");
        assert!(
            is_bad_request(&res),
            "absolute path to a missing file must be rejected, got {res:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_base_is_followed_and_accepted() {
        let (_tmp, s) = storage();
        let real = s.base_path.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("data.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&real, s.base_path.join("link")).unwrap();

        let p = s
            .safe_path("link/data.txt")
            .expect("internal symlink must resolve");
        // Canonicalized to the real location, still inside the base.
        assert_eq!(p, real.join("data.txt"));
        assert!(p.starts_with(&s.base_path));
    }

    #[cfg(unix)]
    #[test]
    fn existing_file_behind_outward_symlink_is_rejected() {
        let (tmp, s) = storage();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, s.base_path.join("evil")).unwrap();

        let res = s.safe_path("evil/secret.txt");
        assert!(
            is_bad_request(&res),
            "reading through an out-of-base symlink must be rejected, got {res:?}"
        );
    }

    /// For a path that does not exist yet, `safe_path` canonicalizes the
    /// deepest existing ancestor: an out-of-base symlink already planted
    /// inside the base is detected even when targeting a NEW file through
    /// it, so a subsequent `put` can no longer create the file on the
    /// symlink's target side.
    #[cfg(unix)]
    #[test]
    fn new_file_behind_outward_symlink_is_rejected() {
        let (tmp, s) = storage();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, s.base_path.join("evil")).unwrap();

        let res = s.safe_path("evil/new-file.txt");
        assert!(
            is_bad_request(&res),
            "creating a new file through an out-of-base symlink must be rejected, got {res:?}"
        );
    }
}
