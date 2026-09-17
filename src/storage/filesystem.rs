use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncWriteExt};

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

    pub fn resolve(&self, path: &str) -> Result<PathBuf, AppError> {
        self.safe_path(path)
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

    /// Written next to its destination as `{name}.part-{uuid}` and renamed
    /// over it, so a re-push swaps inodes and never truncates a reader.
    async fn put(&self, path: &str, data: Bytes) -> Result<(), AppError> {
        let full_path = self.safe_path(path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let part = part_path(&full_path);
        if let Err(e) = write_synced(&part, &data).await {
            let _ = fs::remove_file(&part).await;
            return Err(e.into());
        }
        fs::rename(&part, &full_path).await?;
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

    async fn rename(&self, from: &str, to: &str) -> Result<(), AppError> {
        let from_path = self.safe_path(from)?;
        let to_path = self.safe_path(to)?;
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(&from_path, &to_path).await?;
        Ok(())
    }

    async fn read_stream(
        &self,
        path: &str,
    ) -> Result<(u64, Pin<Box<dyn AsyncRead + Send>>), AppError> {
        let full_path = self.safe_path(path)?;
        let file = match fs::File::open(&full_path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(AppError::NotFound(format!("file not found: {path}")));
            }
            Err(e) => return Err(e.into()),
        };
        let len = file.metadata().await?.len();
        Ok((len, Box::pin(file)))
    }

    async fn remove_stale_parts(
        &self,
        prefix: &str,
        older_than: Duration,
    ) -> Result<u64, AppError> {
        let root = self.safe_path(prefix)?;
        if !root.is_dir() {
            return Ok(0);
        }
        let cutoff = SystemTime::now() - older_than;
        let mut removed = 0;
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if file_type.is_file() && is_stale_part(&entry.path(), cutoff).await? {
                    removed +=
                        vanished_is_fine(fs::remove_file(entry.path()).await)?.is_some() as u64;
                }
            }
        }
        Ok(removed)
    }
}

fn part_path(full_path: &Path) -> PathBuf {
    let mut name = full_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".part-{}", uuid::Uuid::new_v4()));
    full_path.with_file_name(name)
}

async fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path).await?;
    file.write_all(data).await?;
    file.sync_data().await
}

async fn is_stale_part(path: &Path, cutoff: SystemTime) -> Result<bool, AppError> {
    let is_part = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(".part-"));
    if !is_part {
        return Ok(false);
    }
    match vanished_is_fine(fs::metadata(path).await)? {
        Some(meta) => Ok(meta.modified()? < cutoff),
        None => Ok(false),
    }
}

// A part committed (renamed) between the listing and this call is not ours.
fn vanished_is_fine<T>(res: std::io::Result<T>) -> Result<Option<T>, AppError> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
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

    #[tokio::test]
    async fn rename_read_stream_and_stale_parts() {
        use super::StorageBackend;
        use tokio::io::AsyncReadExt;
        let (_tmp, s) = storage();
        let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);

        s.put("c/a/blob.part-1", bytes::Bytes::from_static(b"first"))
            .await
            .unwrap();
        s.rename("c/a/blob.part-1", "c/a/blob").await.unwrap();
        assert!(!s.exists("c/a/blob.part-1").await.unwrap());
        let (len, mut reader) = s.read_stream("c/a/blob").await.unwrap();
        assert_eq!(len, 5);

        s.put("c/a/blob.part-2", bytes::Bytes::from_static(b"second!"))
            .await
            .unwrap();
        s.rename("c/a/blob.part-2", "c/a/blob").await.unwrap();
        let mut held = Vec::new();
        reader.read_to_end(&mut held).await.unwrap();
        assert_eq!(
            held, b"first",
            "a reader opened before the rename drains the old inode"
        );
        assert_eq!(s.get("c/a/blob").await.unwrap().as_ref(), b"second!");
        assert!(matches!(
            s.read_stream("c/missing").await,
            Err(AppError::NotFound(_))
        ));

        s.put("c/b/x.part-old", bytes::Bytes::from_static(b"o"))
            .await
            .unwrap();
        s.put("c/b/x.part-new", bytes::Bytes::from_static(b"n"))
            .await
            .unwrap();
        s.put("c/b/keep.part-me/y", bytes::Bytes::from_static(b"d"))
            .await
            .unwrap();
        for rel in ["c/b/x.part-old", "c/b/keep.part-me/y"] {
            let f = std::fs::File::options()
                .write(true)
                .open(s.resolve(rel).unwrap())
                .unwrap();
            f.set_modified(old_mtime).unwrap();
        }
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(s.remove_stale_parts("c", hour).await.unwrap(), 1);
        assert!(!s.exists("c/b/x.part-old").await.unwrap());
        assert!(
            s.exists("c/b/x.part-new").await.unwrap(),
            "young parts stay"
        );
        assert!(
            s.exists("c/b/keep.part-me/y").await.unwrap(),
            "only file names are matched"
        );
        assert!(s.exists("c/a/blob").await.unwrap());
        assert_eq!(s.remove_stale_parts("nowhere", hour).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn put_swaps_inodes_so_a_reader_keeps_the_old_body() {
        use super::StorageBackend;
        use tokio::io::AsyncReadExt;
        let (_tmp, s) = storage();
        s.put("oci/blob", bytes::Bytes::from_static(b"first"))
            .await
            .unwrap();
        let (len, mut reader) = s.read_stream("oci/blob").await.unwrap();
        assert_eq!(len, 5);

        s.put("oci/blob", bytes::Bytes::from_static(b"the second push"))
            .await
            .unwrap();
        let mut held = Vec::new();
        reader.read_to_end(&mut held).await.unwrap();
        assert_eq!(held, b"first", "never truncated under the reader");
        assert_eq!(s.get("oci/blob").await.unwrap().as_ref(), b"the second push");
        let leftovers: Vec<_> = std::fs::read_dir(s.resolve("oci").unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(leftovers, vec!["blob"], "no part file survives a put");
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
