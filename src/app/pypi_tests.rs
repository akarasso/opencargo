use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::TimeDelta;

use super::*;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::error::AppError;
use crate::ports::reclaim::{Claim, ReclaimStore};
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

const GRACE: Duration = Duration::from_secs(3600);
const WHEEL: &str = "demo-1.0-py3-none-any.whl";

/// A store whose first `revoke_on.len()` commits see the named entries'
/// pins claimed by a reclaimer first, as a placer stalled past its pin would.
struct Revoking {
    inner: Arc<dyn PypiFileStore>,
    reclaim: Arc<dyn ReclaimStore>,
    storage: MemStorage,
    revoke_on: Mutex<Vec<Vec<usize>>>,
    commits: AtomicUsize,
}

impl Revoking {
    async fn revoke(&self, key: &str) {
        let late = Utc::now() + TimeDelta::hours(3);
        self.reclaim
            .enqueue(&[key.to_string()], Utc::now() - TimeDelta::hours(2))
            .await
            .unwrap();
        let claim = self.reclaim.claim(key, GRACE, late, late + TimeDelta::minutes(5)).await.unwrap();
        assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
        self.storage.vanish(key);
    }
}

#[async_trait]
impl PypiFileStore for Revoking {
    async fn publish_file(&self, file: &NewPypiFile<'_>) -> Result<Published, StoreError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        let next = {
            let mut queue = self.revoke_on.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        for i in next.unwrap_or_default() {
            self.revoke(&file.pins[i].physical_key).await;
        }
        self.inner.publish_file(file).await
    }

    async fn file_by_name(&self, r: i64, f: &str) -> Result<Option<PypiFile>, StoreError> {
        self.inner.file_by_name(r, f).await
    }

    async fn project_files(&self, r: i64, p: &str) -> Result<Vec<PypiFile>, StoreError> {
        self.inner.project_files(r, p).await
    }

    async fn list_projects(&self, r: i64) -> Result<Vec<String>, StoreError> {
        self.inner.list_projects(r).await
    }

    async fn set_release_yanked(
        &self,
        r: i64,
        p: &str,
        v: &str,
        reason: Option<&str>,
        yanked: bool,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.inner.set_release_yanked(r, p, v, reason, yanked, now).await
    }

    async fn delete_release(&self, r: i64, p: &str, v: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        self.inner.delete_release(r, p, v, now).await
    }

    async fn delete_project_files(&self, r: i64, p: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        self.inner.delete_project_files(r, p, now).await
    }
}

struct Fx {
    store: FakeDb,
    storage: MemStorage,
    repo: i64,
    prefix: String,
}

impl Fx {
    async fn new() -> Self {
        let db = FakeDb::new();
        let repo = db
            .repositories()
            .create(
                &RepoSpec {
                    name: "py",
                    kind: RepoKind::Hosted,
                    format: Format::Pypi,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
        let incarnation = db.repositories().incarnation(repo.id).await.unwrap().unwrap();
        Self {
            storage: MemStorage::new(),
            prefix: layout::incarnation_prefix(&incarnation),
            store: db,
            repo: repo.id,
        }
    }

    fn publisher_over(&self, files: Arc<dyn PypiFileStore>) -> PublishPypiFile {
        let placer = Arc::new(Placer::new(self.store.reclaim(), Arc::new(self.storage.clone())));
        PublishPypiFile::new(files, self.store.repositories(), placer)
    }

    fn publisher(&self) -> PublishPypiFile {
        self.publisher_over(self.store.pypi())
    }

    fn revoking(&self, revoke_on: Vec<Vec<usize>>) -> Arc<Revoking> {
        Arc::new(Revoking {
            inner: self.store.pypi(),
            reclaim: self.store.reclaim(),
            storage: self.storage.clone(),
            revoke_on: Mutex::new(revoke_on),
            commits: AtomicUsize::new(0),
        })
    }

    fn upload(&self, filename: &'static str, body: &'static [u8]) -> Upload<'static> {
        Upload {
            repository: self.repo,
            project: "demo",
            summary: Some("a demo"),
            version: "1",
            metadata_json: "{}",
            filename,
            packagetype: "bdist_wheel",
            requires_python: None,
            bytes: Bytes::from_static(body),
            metadata: filename
                .ends_with(".whl")
                .then(|| Bytes::from_static(b"Metadata-Version: 2.1\nName: demo\nVersion: 1.0\n")),
        }
    }

    async fn served(&self, key: &str) -> Vec<u8> {
        crate::storage::StorageBackend::get(&self.storage, key).await.unwrap().to_vec()
    }
}

fn created(done: Uploaded) -> PypiFile {
    match done {
        Uploaded::Created(published) => published.file,
        Uploaded::Unchanged(file) => panic!("expected a new file, got {file:?}"),
    }
}

#[tokio::test]
async fn artifact_and_metadata_are_two_entries_of_one_placement() {
    let fx = Fx::new().await;
    let file = created(fx.publisher().run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap());
    let sha = sha256_hex(b"wheel");
    let artifact = layout::hosted_key(&fx.prefix, "demo", &sha, WHEEL);
    assert_eq!(layout::logical_key(&file.key), artifact);
    let metadata = file.metadata_key.clone().expect("a wheel's .metadata is placed");
    assert_eq!(layout::logical_key(&metadata), format!("{artifact}.metadata"));
    assert_eq!(fx.served(&file.key).await, b"wheel");
    assert!(fx.served(&metadata).await.starts_with(b"Metadata-Version"));
    assert_eq!(file.metadata_sha256, Some(sha256_hex(b"Metadata-Version: 2.1\nName: demo\nVersion: 1.0\n")));
    assert!(!file.key.contains("/py/"), "no repository name in a key");
    assert!(fx.storage.deleted().is_empty());

    let sdist = created(fx.publisher().run(fx.upload("demo-1.0.tar.gz", b"sdist"), Utc::now()).await.unwrap());
    assert!(sdist.metadata_key.is_none(), "an sdist has no .metadata");
}

#[tokio::test]
async fn metadata_key_is_recorded_not_derived() {
    let fx = Fx::new().await;
    let file = created(fx.publisher().run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap());
    let metadata = file.metadata_key.clone().unwrap();
    assert_ne!(metadata, format!("{}.metadata", file.key), "a generation of its own");
    let generation = |k: &str| k.rsplit_once('~').map(|(_, g)| g.to_string()).unwrap();
    assert_ne!(generation(&metadata), generation(&file.key));
    let stored = fx.store.pypi().file_by_name(fx.repo, WHEEL).await.unwrap().unwrap();
    assert_eq!(stored.metadata_key, Some(metadata), "the row holds the physical key the pin named");
}

#[tokio::test]
async fn superseded_artifact_replaces_metadata_too() {
    for (revoked, why) in [(vec![0], "artifact"), (vec![1], "metadata"), (vec![0, 1], "both")] {
        let fx = Fx::new().await;
        let store = fx.revoking(vec![revoked.clone()]);
        let file = created(
            fx.publisher_over(store.clone())
                .run(fx.upload(WHEEL, b"wheel"), Utc::now())
                .await
                .unwrap(),
        );
        assert_eq!(store.commits.load(Ordering::SeqCst), 2, "{why}: superseded once");
        let metadata = file.metadata_key.clone().unwrap();
        assert_eq!(fx.served(&file.key).await, b"wheel", "{why}");
        assert!(fx.served(&metadata).await.starts_with(b"Metadata-Version"), "{why}");
        let row = fx.store.pypi().file_by_name(fx.repo, WHEEL).await.unwrap().unwrap();
        assert_eq!((row.key, row.metadata_key), (file.key.clone(), Some(metadata)), "{why}: two keys, one row");
        assert!(fx.storage.deleted().is_empty(), "{why}: nothing deleted inline");
    }
}

#[tokio::test]
async fn pypi_superseded_publish_copies_or_503() {
    let fx = Fx::new().await;
    let store = fx.revoking(vec![vec![0]]);
    let file = created(fx.publisher_over(store).run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap());
    assert_eq!(fx.served(&file.key).await, b"wheel", "replayed from the upload's own bytes");

    let fx = Fx::new().await;
    let store = fx.revoking(vec![vec![0], vec![0], vec![0]]);
    let refused = fx
        .publisher_over(store)
        .run(fx.upload(WHEEL, b"wheel"), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Unavailable), "{refused:?}");
    assert!(matches!(AppError::from(refused), AppError::ServiceUnavailable(_)));
    assert!(fx.store.pypi().file_by_name(fx.repo, WHEEL).await.unwrap().is_none(), "nothing recorded");
    assert!(fx.storage.deleted().is_empty(), "the residue is enqueued, never deleted");
    assert!(!fx.store.candidates().is_empty());
}

#[tokio::test]
async fn conflict_loser_enqueues_and_deletes_nothing() {
    let fx = Fx::new().await;
    let first = created(fx.publisher().run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap());
    match fx.publisher().run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap() {
        Uploaded::Unchanged(existing) => assert_eq!(existing.key, first.key),
        other => panic!("same bytes are an idempotent success: {other:?}"),
    }
    let refused = fx
        .publisher()
        .run(fx.upload(WHEEL, b"other bytes"), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Store(StoreError::Conflict)), "{refused:?}");
    assert!(fx.storage.deleted().is_empty());
    let queued = fx.store.candidates();
    assert!(queued.iter().any(|k| layout::logical_key(k).contains(&sha256_hex(b"other bytes"))));
    assert_eq!(fx.served(&first.key).await, b"wheel", "the winner's bytes stay");
}

#[tokio::test]
async fn yank_and_delete_act_on_the_whole_release() {
    let fx = Fx::new().await;
    let wheel = created(fx.publisher().run(fx.upload(WHEEL, b"wheel"), Utc::now()).await.unwrap());
    created(fx.publisher().run(fx.upload("demo-1.0.tar.gz", b"sdist"), Utc::now()).await.unwrap());
    YankRelease::new(fx.store.pypi())
        .run(fx.repo, "demo", "1", Some("broken"), true, Utc::now())
        .await
        .unwrap();
    let files = fx.store.pypi().project_files(fx.repo, "demo").await.unwrap();
    assert!(files.iter().all(|f| f.yanked && f.yanked_reason.as_deref() == Some("broken")));

    let released = DeleteRelease::new(fx.store.pypi())
        .run(fx.repo, "demo", Some("1"), Utc::now())
        .await
        .unwrap();
    assert!(released.contains(&wheel.key) && released.contains(wheel.metadata_key.as_ref().unwrap()));
    assert!(fx.storage.deleted().is_empty(), "candidates, not deletions");
    assert!(fx.served(&wheel.key).await == b"wheel");
    let queued = fx.store.candidates();
    assert!(released.iter().all(|k| queued.contains(k)));
}
