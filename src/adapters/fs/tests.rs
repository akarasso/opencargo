use super::*;

fn storage() -> (tempfile::TempDir, FilesystemStorage) {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let storage = FilesystemStorage::new(tmp.path().join("base"), StoreIdentity("artifacts".into()));
    (tmp, storage)
}

mod contract {
    use super::*;

    crate::storage_contract!(async {
        let (tmp, s) = storage();
        let s: Arc<dyn StorageBackend> = Arc::new(s);
        (tmp, s)
    });
}

fn rejected(res: Result<PathBuf, StorageError>) -> bool {
    matches!(res, Err(StorageError::InvalidPath(_)))
}

#[test]
fn nested_keys_resolve_under_the_base() {
    let (_tmp, s) = storage();
    let p = s.safe_path("npm/my-repo/pkg/pkg-1.0.0.tgz").unwrap();
    assert!(p.starts_with(&s.base_path));
    let p = s.safe_path("%2e%2e/escape.txt").unwrap();
    assert!(p.starts_with(&s.base_path), "a literal odd name, not a traversal");
}

#[test]
fn traversal_and_absolute_keys_are_rejected() {
    let (_tmp, s) = storage();
    for attempt in [
        "..",
        "../escape.txt",
        "a/../../etc/passwd",
        "foo..bar.txt",
        "/etc/passwd",
        "/definitely/not/existing/xyz.txt",
    ] {
        assert!(rejected(s.safe_path(attempt)), "{attempt:?}");
    }
}

#[cfg(unix)]
#[test]
fn symlink_inside_base_is_followed_and_accepted() {
    let (_tmp, s) = storage();
    let real = s.base_path.join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("data.txt"), b"x").unwrap();
    std::os::unix::fs::symlink(&real, s.base_path.join("link")).unwrap();
    let p = s.safe_path("link/data.txt").unwrap();
    assert!(p.starts_with(&s.base_path));
}

#[cfg(unix)]
#[test]
fn files_behind_an_outward_symlink_are_rejected() {
    let (tmp, s) = storage();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"s").unwrap();
    std::os::unix::fs::symlink(&outside, s.base_path.join("evil")).unwrap();
    assert!(rejected(s.safe_path("evil/secret.txt")));
    assert!(rejected(s.safe_path("evil/new-file.txt")));
}

#[tokio::test]
async fn abandoned_scratch_and_legacy_parts_are_swept_by_age() {
    let (_tmp, s) = storage();
    let w = s.writer("x/y").await.unwrap();
    std::mem::forget(w);
    std::fs::create_dir_all(s.base_path.join("c/b")).unwrap();
    std::fs::write(s.base_path.join("c/b/x.part-old"), b"o").unwrap();
    std::fs::write(s.base_path.join("c/b/keep"), b"k").unwrap();
    let now = Utc::now();
    let hour = Duration::from_secs(3600);
    assert_eq!(s.sweep_abandoned(hour, now).await.unwrap(), 0, "young residue stays");
    let later = now + chrono::TimeDelta::hours(2);
    assert_eq!(s.sweep_abandoned(hour, later).await.unwrap(), 2);
    assert!(s.stat("c/b/keep").await.unwrap().is_some());
    assert!(!s.base_path.join("c/b/x.part-old").exists());
}
