use bytes::Bytes;
use chrono::TimeDelta;

use super::*;
use crate::app::repositories::CreateRepository;
use crate::domain::layout;
use crate::app::reclaim::ReclaimPolicy;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::ports::packages::{NameMatch, NewRelease};
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

const GRACE: Duration = Duration::from_secs(3600);

fn sha(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

async fn publish(fakes: &FakeDb, key: &str) {
    let repo = fakes
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
            Utc::now(),
        )
        .await
        .unwrap();
    fakes
        .packages()
        .publish_version(&NewRelease {
            dependencies: &[],
            repository: repo.id,
            package: "p",
            match_name: NameMatch::Exact,
            description: None,
            readme: None,
            version: "1.0.0",
            metadata_json: "{}",
            checksum_sha1: None,
            checksum_sha256: None,
            integrity: None,
            size: 1,
            tarball_path: key,
            dist_tags: &[],
            pins: &[],
            now: Utc::now(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_lists_missing_keys_and_orphans_outside_the_grace_window() {
    let fakes = FakeDb::new();
    let storage = MemStorage::new();
    publish(&fakes, "npm/r/p/p-1.0.0.tgz").await;
    storage.put("stray/object", Bytes::from_static(b"x")).await.unwrap();
    let verify = VerifyStorage::new(
        fakes.referenced(),
        fakes.reclaim(),
        Arc::new(storage.clone()),
        GRACE,
    );

    let now = verify.run(listing(), Utc::now()).await.unwrap();
    assert_eq!(now.missing, vec!["npm/r/p/p-1.0.0.tgz".to_string()]);
    assert!(now.orphans.is_empty(), "an object inside the grace window is in flight");
    assert_eq!(now.objects, 1);

    let later = verify.run(listing(), Utc::now() + TimeDelta::hours(3)).await.unwrap();
    assert_eq!(later.orphans, vec!["stray/object".to_string()]);
    assert!(verify
        .run(Verify::default(), Utc::now() + TimeDelta::hours(3))
        .await
        .unwrap()
        .orphans
        .is_empty());
}

fn listing() -> Verify {
    Verify {
        orphans: true,
        ..Verify::default()
    }
}

/// C-7: a store that keeps noncurrent versions puts a referenced key's
/// bytes back; one that keeps none refuses the repair rather than pretend.
#[tokio::test]
async fn repair_puts_a_noncurrent_version_back_and_refuses_without_one() {
    let fakes = FakeDb::new();
    let storage = MemStorage::new().versioned();
    publish(&fakes, "npm/r/p/p-1.0.0.tgz").await;
    storage
        .put("npm/r/p/p-1.0.0.tgz", Bytes::from_static(b"x"))
        .await
        .unwrap();
    storage.delete("npm/r/p/p-1.0.0.tgz").await.unwrap();
    let verify = VerifyStorage::new(
        fakes.referenced(),
        fakes.reclaim(),
        Arc::new(storage.clone()),
        GRACE,
    );
    let repair = Verify {
        repair: true,
        ..Verify::default()
    };
    let report = verify.run(repair, Utc::now()).await.unwrap();
    assert_eq!(report.repaired, vec!["npm/r/p/p-1.0.0.tgz".to_string()]);
    assert!(report.missing.is_empty());
    assert!(storage.contains("npm/r/p/p-1.0.0.tgz"));

    let plain = VerifyStorage::new(
        fakes.referenced(),
        fakes.reclaim(),
        Arc::new(MemStorage::new()),
        GRACE,
    );
    assert!(matches!(
        plain.run(repair, Utc::now()).await,
        Err(OpsError::NoVersions)
    ));
}

/// I7: what a rollback left behind under a live incarnation goes to the
/// queue, which claims before it deletes; a key under nothing live does not.
#[tokio::test]
async fn a_verify_after_a_rollback_queues_the_orphans_of_live_incarnations() {
    let fakes = FakeDb::new();
    let storage = MemStorage::new();
    let prefix = hosted(&fakes, &storage, "r").await;
    let placed = format!("{prefix}/p/ab/p-1.0.0.tgz~gone");
    storage.put(&placed, Bytes::from_static(b"x")).await.unwrap();
    storage.put("elsewhere/object", Bytes::from_static(b"x")).await.unwrap();
    fakes.reclaim().new_epoch(0).await.unwrap();

    let settle = SettleEpoch::new(
        fakes.reclaim(),
        fakes.referenced(),
        Arc::new(storage.clone()),
        GRACE,
    );
    let later = Utc::now() + TimeDelta::hours(3);
    let report = settle.run(later).await.unwrap().expect("a verify was owed");
    assert_eq!(report.enqueued, vec![placed.clone()]);
    assert!(
        report.orphans.contains(&"elsewhere/object".to_string()),
        "reported, never queued: it belongs to no live incarnation"
    );
    assert!(
        !fakes.reclaim().epoch().await.unwrap().verify_pending,
        "the verify lifts its own refusal"
    );
    assert!(settle.run(later).await.unwrap().is_none(), "nothing owed twice");
}

struct Pair {
    source: MemStorage,
    target: MemStorage,
}

impl Pair {
    fn migrate(&self) -> MigrateStorage {
        MigrateStorage::new(Arc::new(self.source.clone()), Arc::new(self.target.clone()))
    }
}

async fn pair() -> (Pair, String) {
    let pair = Pair {
        source: MemStorage::new(),
        target: MemStorage::new(),
    };
    let hosted = layout::physical_key(&layout::hosted_key("r/i", "p", &sha(b"tarball"), "p.tgz"), "g");
    pair.source.put(&hosted, Bytes::from_static(b"tarball")).await.unwrap();
    pair.source
        .put("r/i/_proxy/npm/ab/key", Bytes::from_static(b"upstream-1"))
        .await
        .unwrap();
    (pair, hosted)
}

#[tokio::test]
async fn migrate_copies_everything_then_skips_only_what_it_can_prove() {
    let (pair, hosted) = pair().await;
    let dry = pair.migrate().run(true).await.unwrap();
    assert_eq!((dry.copied, dry.skipped), (2, 0));
    assert!(pair.target.keys().is_empty(), "a dry run writes nothing");

    let first = pair.migrate().run(false).await.unwrap();
    assert_eq!((first.copied, first.skipped, first.bytes), (2, 0, 17));
    assert_eq!(pair.target.get(&hosted).await.unwrap().as_ref(), b"tarball");

    let again = pair.migrate().run(false).await.unwrap();
    assert_eq!((again.copied, again.skipped), (1, 1), "the digested key is proven, the proxy key is not");

    pair.target.put(&hosted, Bytes::from_static(b"tarbalL")).await.unwrap();
    let healed = pair.migrate().run(false).await.unwrap();
    assert_eq!(healed.skipped, 0, "a same-size body that does not hash to its key is copied again");
    assert_eq!(pair.target.get(&hosted).await.unwrap().as_ref(), b"tarball");
}

#[tokio::test]
async fn migrate_recopies_same_size_proxy_key() {
    let (pair, _) = pair().await;
    pair.migrate().run(false).await.unwrap();
    pair.source
        .put("r/i/_proxy/npm/ab/key", Bytes::from_static(b"upstream-2"))
        .await
        .unwrap();
    pair.migrate().run(false).await.unwrap();
    assert_eq!(
        pair.target.get("r/i/_proxy/npm/ab/key").await.unwrap().as_ref(),
        b"upstream-2"
    );
}

#[tokio::test]
async fn reclaim_prefix_without_repository_row() {
    let fakes = FakeDb::new();
    let storage = MemStorage::new();
    for key in ["npm/gone/p/a.tgz", "npm/gone/p/b.tgz", "npm/gone2/kept.tgz"] {
        storage.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    let reclaim = ReclaimOrphans::new(
        fakes.reclaim(),
        fakes.referenced(),
        Arc::new(storage.clone()),
        ReclaimPolicy {
            grace: GRACE,
            ..ReclaimPolicy::default()
        },
    );
    let report = ReclaimPrefix::new(fakes.reclaim(), reclaim, GRACE)
        .run("npm/gone", Utc::now())
        .await
        .unwrap();
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(storage.keys(), vec!["npm/gone2/kept.tgz".to_string()]);
}

#[tokio::test]
async fn reclaim_prefix_refuses_a_referenced_prefix() {
    let fakes = FakeDb::new();
    let storage = MemStorage::new();
    publish(&fakes, "npm/r/p/p-1.0.0.tgz").await;
    storage.put("npm/r/p/p-1.0.0.tgz", Bytes::from_static(b"x")).await.unwrap();
    let reclaim = ReclaimOrphans::new(fakes.reclaim(), fakes.referenced(), Arc::new(storage.clone()), ReclaimPolicy::default());
    let report = ReclaimPrefix::new(fakes.reclaim(), reclaim, GRACE)
        .run("npm/r", Utc::now())
        .await
        .unwrap();
    assert_eq!((report.reclaimed, report.referenced), (0, 1));
    assert!(storage.contains("npm/r/p/p-1.0.0.tgz"));
}

/// A hosted repository, and the prefix of its incarnation.
async fn hosted(fakes: &FakeDb, storage: &MemStorage, name: &str) -> String {
    let repo = CreateRepository::new(
        fakes.repositories(),
        fakes.audit(),
        crate::server::event_bus(),
    )
    .guarding(Arc::new(storage.clone()))
    .run(
        &RepoSpec {
            name,
            kind: RepoKind::Hosted,
            format: Format::Npm,
            visibility: Visibility::Public,
            upstream: None,
            members: &[],
        },
        &crate::app::audit::Actor {
            user_id: Some(1),
            username: "root",
            admin: true,
            scoped: false,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    let incarnation = fakes.repositories().incarnation(repo.id).await.unwrap().unwrap();
    layout::incarnation_prefix(&incarnation)
}
