use futures_util::stream;

use super::*;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

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
                    name: "files",
                    kind: RepoKind::Hosted,
                    format: Format::Raw,
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

    fn putter(&self) -> PutRawFile {
        let placer = Arc::new(Placer::new(self.store.reclaim(), Arc::new(self.storage.clone())));
        PutRawFile::new(
            self.store.raw(),
            self.store.repositories(),
            Arc::new(self.storage.clone()),
            placer,
        )
    }

    fn deposit<'a>(&self, path: &'a str, declared: Option<&'a str>) -> Deposit<'a> {
        Deposit {
            repository: self.repo,
            path,
            content_type: Some("application/gzip"),
            declared_sha256: declared,
            principal: "ci",
        }
    }

    async fn put(&self, path: &str, body: &'static [u8]) -> Result<Deposited, RawError> {
        self.putter()
            .run(self.deposit(path, None), stream_of(body), Utc::now())
            .await
    }

    async fn served(&self, key: &str) -> Vec<u8> {
        crate::storage::StorageBackend::get(&self.storage, key)
            .await
            .unwrap()
            .to_vec()
    }
}

fn stream_of(body: &'static [u8]) -> Body {
    Box::pin(stream::iter(
        body.chunks(3).map(|c| Ok(Bytes::copy_from_slice(c))),
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn a_file_lands_under_a_hosted_key_of_its_incarnation() {
    let fx = Fx::new().await;
    let stored = fx.put("dist/tool.bin", b"payload").await.unwrap();
    let Deposited::Stored { created, file } = stored else {
        panic!("a first put creates");
    };
    assert!(created);
    let sha = sha256_hex(b"payload");
    assert_eq!(file.sha256, sha);
    assert_eq!(file.size, 7);
    assert_eq!(file.content_type.as_deref(), Some("application/gzip"));
    assert_eq!(
        layout::logical_key(&file.physical_key),
        layout::hosted_key(&fx.prefix, "dist/tool.bin", &sha, "tool.bin")
    );
    assert!(!file.physical_key.contains("/files/"), "no repository name in a key");
    assert_eq!(fx.served(&file.physical_key).await, b"payload");
    assert!(fx.storage.deleted().iter().all(|k| k.contains("_drafts")), "only its own draft");
}

#[tokio::test]
async fn the_same_bytes_at_the_same_path_write_nothing_new() {
    let fx = Fx::new().await;
    let first = fx.put("dist/tool.bin", b"payload").await.unwrap();
    let again = fx.put("dist/tool.bin", b"payload").await.unwrap();
    let Deposited::Unchanged(file) = again else {
        panic!("the same bytes are unchanged, got {again:?}");
    };
    assert_eq!(&file, first.file());
    assert_eq!(fx.store.candidates(), Vec::<String>::new(), "nothing was released");
}

#[tokio::test]
async fn other_bytes_at_one_path_replace_it_and_enqueue_the_key_it_held() {
    let fx = Fx::new().await;
    let first = fx.put("dist/tool.bin", b"payload").await.unwrap();
    let held = first.file().physical_key.clone();
    let second = fx.put("dist/tool.bin", b"other").await.unwrap();
    let Deposited::Stored { created, file } = &second else {
        panic!("other bytes are a write");
    };
    assert!(!created, "the path already held a file");
    assert_ne!(file.physical_key, held);
    assert_eq!(fx.store.candidates(), vec![held.clone()]);
    assert!(fx.storage.contains(&held), "enqueued, never deleted");
    assert_eq!(fx.served(&file.physical_key).await, b"other");
}

#[tokio::test]
async fn a_declared_checksum_that_does_not_match_records_nothing() {
    let fx = Fx::new().await;
    let wrong = "b".repeat(64);
    let refused = fx
        .putter()
        .run(
            fx.deposit("dist/tool.bin", Some(&wrong)),
            stream_of(b"payload"),
            Utc::now(),
        )
        .await;
    assert!(matches!(refused, Err(RawError::Mismatch { .. })), "{refused:?}");
    assert!(fx.store.raw().file(fx.repo, "dist/tool.bin").await.unwrap().is_none());
    assert!(fx.storage.keys().is_empty(), "the draft was dropped");

    let right = sha256_hex(b"payload");
    let ok = fx
        .putter()
        .run(
            fx.deposit("dist/tool.bin", Some(&right)),
            stream_of(b"payload"),
            Utc::now(),
        )
        .await;
    assert!(matches!(ok, Ok(Deposited::Stored { .. })), "{ok:?}");
}

#[tokio::test]
async fn deleting_a_path_enqueues_its_key_and_deletes_nothing() {
    let fx = Fx::new().await;
    let file = fx.put("dist/tool.bin", b"payload").await.unwrap();
    let key = file.file().physical_key.clone();
    let released = DeleteRawFile::new(fx.store.raw())
        .run(fx.repo, "dist/tool.bin", Utc::now())
        .await
        .unwrap();
    assert_eq!(released, vec![key.clone()]);
    assert_eq!(fx.store.candidates(), vec![key.clone()]);
    assert!(fx.storage.contains(&key), "the reclaimer deletes, this use case does not");
    assert!(fx.store.raw().file(fx.repo, "dist/tool.bin").await.unwrap().is_none());
}
