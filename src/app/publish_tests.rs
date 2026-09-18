use super::*;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::testing::fakes::{FakeDb, PortId};
use crate::testing::storage::MemStorage;

fn artifact<'a>(repository: i64, version: &'a str, tags: &'a [String]) -> Artifact<'a> {
    Artifact {
        repository,
        package: "left-pad",
        match_name: NameMatch::Exact,
        description: Some("a package"),
        readme: None,
        version,
        metadata_json: "{}",
        checksum_sha1: None,
        checksum_sha256: None,
        integrity: None,
        filename: "left-pad-1.0.0.tgz",
        dist_tags: tags,
        bytes: Bytes::from_static(b"tarball"),
    }
}

async fn setup() -> (FakeDb, MemStorage, i64, PublishVersion) {
    let db = FakeDb::new();
    let repo = db
        .repositories()
        .create(
            &RepoSpec {
                name: "r",
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
    let storage = MemStorage::new();
    let placer = Arc::new(Placer::new(db.reclaim(), Arc::new(storage.clone())));
    let publish = PublishVersion::new(db.packages(), db.repositories(), placer);
    (db, storage, repo.id, publish)
}

#[tokio::test]
async fn a_version_lands_under_a_hosted_key_of_its_incarnation() {
    let (db, storage, repo, publish) = setup().await;
    let landed = publish.run(artifact(repo, "1.0.0", &[]), Utc::now()).await.unwrap();
    let incarnation = db.repositories().incarnation(repo).await.unwrap().unwrap();
    let sha = format!("{:x}", Sha256::digest(b"tarball"));
    let logical = layout::hosted_key(
        &layout::incarnation_prefix(&incarnation),
        "left-pad",
        &sha,
        "left-pad-1.0.0.tgz",
    );
    assert!(landed.version.tarball_path.starts_with(&format!("{logical}~")));
    assert!(storage.contains(&landed.version.tarball_path));
    assert!(!landed.version.tarball_path.contains("/r/"), "no repository name in a new key");
}

#[tokio::test]
async fn a_second_publish_of_the_same_version_is_a_conflict() {
    let (_db, _storage, repo, publish) = setup().await;
    let tags = vec!["latest".to_string()];
    publish.run(artifact(repo, "1.0.0", &tags), Utc::now()).await.unwrap();
    let refused = publish
        .run(artifact(repo, "1.0.0", &tags), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Store(StoreError::Conflict)));
    assert!(matches!(AppError::from(refused), AppError::Conflict(_)));
}

/// The package row is upserted inside the transaction, so a refused
/// version leaves no package behind for the search index to find.
#[tokio::test]
async fn a_refused_write_leaves_no_package_row() {
    let (db, storage, repo, publish) = setup().await;
    db.fail_next(PortId::Packages, StoreError::Unavailable);
    let refused = publish
        .run(artifact(repo, "1.0.0", &[]), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Store(StoreError::Unavailable)));
    assert!(db
        .packages()
        .package(repo, "left-pad", NameMatch::Exact)
        .await
        .unwrap()
        .is_none());
    assert!(storage.deleted().is_empty(), "the orphan is enqueued, not deleted");
    assert_eq!(db.candidates().len(), 1);
}

#[tokio::test]
async fn a_retired_repository_takes_no_publish() {
    let (db, storage, repo, publish) = setup().await;
    db.repositories().retire("r", Utc::now()).await.unwrap();
    let refused = publish
        .run(artifact(repo, "1.0.0", &[]), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Retired), "{refused:?}");
    assert!(storage.keys().is_empty(), "nothing written");
}
