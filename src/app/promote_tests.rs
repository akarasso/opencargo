use chrono::TimeDelta;

use super::*;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::testing::fakes::{FakeDb, PortId};
use crate::testing::storage::MemStorage;

struct Fx {
    store: FakeDb,
    storage: MemStorage,
    target: Repository,
}

impl Fx {
    async fn new() -> Self {
        let store = FakeDb::new();
        let target = store
            .repositories()
            .create(
                &RepoSpec {
                    name: "npm-prod",
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                },
                DateTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        Self {
            store,
            storage: MemStorage::new(),
            target,
        }
    }

    fn promote(&self) -> PromoteVersion {
        let storage: Arc<dyn StorageBackend> = Arc::new(self.storage.clone());
        PromoteVersion::new(
            self.store.packages(),
            self.store.repositories(),
            storage.clone(),
            Arc::new(Placer::new(self.store.reclaim(), storage)),
        )
    }

    async fn staged(&self, sha: Option<&str>) -> Version {
        let path = "npm/npm-stage/left-pad/left-pad-1.0.0.tgz";
        self.storage.put(path, Bytes::from_static(b"tgz!")).await.unwrap();
        Version {
            id: 7,
            package_id: 3,
            version: "1.0.0".to_string(),
            metadata_json: "{}".to_string(),
            checksum_sha1: None,
            checksum_sha256: sha.map(str::to_string),
            integrity: None,
            size: 4,
            tarball_path: path.to_string(),
            published_at: DateTime::UNIX_EPOCH,
            yanked: false,
        }
    }
}

fn request<'a>(source: &'a Version, target: &'a Repository, tags: &'a [String]) -> Request<'a> {
    Request {
        source,
        target,
        package: "left-pad",
        description: Some("a package"),
        metadata_json: "{}",
        dist_tags: tags,
        details_json: r#"{"from":"npm-stage","to":"npm-prod"}"#,
    }
}

fn alex() -> Promoter<'static> {
    Promoter {
        user_id: Some(1),
        username: "alex",
    }
}

const SHA: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// The four writes land together, and the audit row names the promoter.
#[tokio::test]
async fn a_promotion_carries_the_blob_the_rows_and_its_tags() {
    let fx = Fx::new().await;
    let source = fx.staged(Some(SHA)).await;
    let tags = vec!["latest".to_string()];

    let promoted = fx
        .promote()
        .run(request(&source, &fx.target, &tags), alex(), DateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert!(promoted.tarball_path.contains(&format!("/left-pad/{SHA}/left-pad-1.0.0.tgz~")));
    assert_eq!(fx.storage.get(&promoted.tarball_path).await.unwrap().as_ref(), b"tgz!");
    assert!(fx.storage.contains(&source.tarball_path), "the source is kept");
    let tags = fx.store.packages().dist_tags(promoted.package_id).await.unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(
        fx.store.audit_rows(),
        vec![(Some(1), "package.promote".to_string(), "left-pad@1.0.0".to_string())]
    );
}

/// A failed metadata transaction enqueues its copy and deletes nothing.
#[tokio::test]
async fn a_failed_promotion_enqueues_the_artifact_it_copied() {
    let fx = Fx::new().await;
    let source = fx.staged(Some(SHA)).await;
    fx.store.fail_next(PortId::Packages, StoreError::Unavailable);

    let refused = fx
        .promote()
        .run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
        .await
        .unwrap_err();

    assert!(matches!(refused, PromoteError::Store(StoreError::Unavailable)));
    assert!(fx.storage.deleted().is_empty());
    let queued = fx.store.candidates();
    assert_eq!(queued.len(), 1);
    assert!(fx.storage.contains(&queued[0]), "enqueued, not deleted");
    assert!(fx.store.audit_rows().is_empty());
}

#[tokio::test]
async fn concurrent_promotes_keep_the_winner_bytes() {
    let fx = Fx::new().await;
    let source = fx.staged(Some(SHA)).await;
    let (left, right) = (fx.promote(), fx.promote());
    let (a, b) = tokio::join!(
        left.run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH),
        right.run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH),
    );
    let (winner, loser) = match (a, b) {
        (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
        other => panic!("one wins, one conflicts: {other:?}"),
    };
    assert!(is_conflict(&loser), "{loser:?}");
    let report = crate::app::reclaim::ReclaimOrphans::new(
        fx.store.reclaim(),
        fx.store.referenced(),
        Arc::new(fx.storage.clone()),
        crate::app::reclaim::ReclaimPolicy::default(),
    )
    .run(Utc::now() + TimeDelta::hours(3))
    .await;
    assert!(report.reclaimed <= 1);
    assert_eq!(fx.storage.get(&winner.tarball_path).await.unwrap().as_ref(), b"tgz!");
}

#[tokio::test]
async fn legacy_promote_goes_draft_pin_relocate() {
    let fx = Fx::new().await;
    let source = fx.staged(None).await;

    let promoted = fx
        .promote()
        .run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
        .await
        .unwrap();

    let sha = format!("{:x}", Sha256::digest(b"tgz!"));
    assert!(
        promoted.tarball_path.contains(&format!("/{sha}/")),
        "the digest is computed on the way into the draft: {}",
        promoted.tarball_path
    );
    assert_eq!(fx.storage.get(&promoted.tarball_path).await.unwrap().as_ref(), b"tgz!");
    assert!(
        !fx.storage.keys().iter().any(|k| k.contains("/_drafts/")),
        "no draft outlives the promote: {:?}",
        fx.storage.keys()
    );
}

#[tokio::test]
async fn claimed_draft_fails_promote_retryably() {
    let fx = Fx::new().await;
    let source = fx.staged(None).await;
    fx.storage.fail_next("relocate");

    let refused = fx
        .promote()
        .run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
        .await
        .unwrap_err();

    assert!(matches!(
        crate::error::AppError::from(refused),
        crate::error::AppError::ServiceUnavailable(_)
    ));
    assert!(fx.storage.contains(&source.tarball_path), "the source row's bytes are untouched");
    let again = fx
        .promote()
        .run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
        .await;
    assert!(again.is_ok(), "a retry goes through: {again:?}");
}

#[tokio::test]
async fn failed_promote_during_identical_publish_keeps_bytes() {
    let fx = Fx::new().await;
    let sha = format!("{:x}", Sha256::digest(b"tgz!"));
    let recorded = fx.staged(Some(&sha)).await;
    let published = fx
        .promote()
        .run(request(&recorded, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
        .await
        .unwrap();

    for source in [fx.staged(Some(&sha)).await, fx.staged(None).await] {
        let refused = fx
            .promote()
            .run(request(&source, &fx.target, &[]), alex(), DateTime::UNIX_EPOCH)
            .await
            .unwrap_err();
        assert!(is_conflict(&refused), "{refused:?}");
        assert_eq!(
            fx.store.candidates(),
            vec![published.tarball_path.clone()],
            "the loser wrote onto the reused generation and enqueued it"
        );
        let report = crate::app::reclaim::ReclaimOrphans::new(
            fx.store.reclaim(),
            fx.store.referenced(),
            Arc::new(fx.storage.clone()),
            crate::app::reclaim::ReclaimPolicy::default(),
        )
        .run(Utc::now() + TimeDelta::hours(3))
        .await;
        assert_eq!((report.reclaimed, report.referenced), (0, 1), "{report:?}");
        assert_eq!(fx.storage.get(&published.tarball_path).await.unwrap().as_ref(), b"tgz!");
    }
}
