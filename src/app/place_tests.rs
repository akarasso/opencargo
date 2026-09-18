use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::TimeDelta;

use super::*;
use crate::app::publish::{Artifact, PublishError, PublishVersion};
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::ports::packages::{NameMatch, NewRelease};
use crate::ports::reclaim::Claim;
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

const GRACE: Duration = Duration::from_secs(3600);

struct Fx {
    store: FakeDb,
    storage: MemStorage,
    repo: i64,
    prefix: String,
}

impl Fx {
    async fn new() -> Self {
        let store = FakeDb::new();
        let repo = store
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
        let incarnation = store.repositories().incarnation(repo.id).await.unwrap().unwrap();
        Self {
            storage: MemStorage::new(),
            prefix: layout::incarnation_prefix(&incarnation),
            store,
            repo: repo.id,
        }
    }

    fn placer(&self) -> Placer {
        Placer::new(self.store.reclaim(), Arc::new(self.storage.clone()))
    }

    fn publisher(&self) -> PublishVersion {
        PublishVersion::new(
            self.store.packages(),
            self.store.repositories(),
            Arc::new(self.placer()),
        )
    }

    fn logical(&self) -> String {
        layout::hosted_key(&self.prefix, "p", "abc", "p-1.0.0.tgz")
    }

    /// What a reclaimer that won a claim on `key` does: revoke, then delete.
    async fn reclaim_now(&self, key: &str, delete: bool) {
        let reclaim = self.store.reclaim();
        let late = Utc::now() + TimeDelta::hours(3);
        reclaim.enqueue(&[key.to_string()], Utc::now() - TimeDelta::hours(2)).await.unwrap();
        let claim = reclaim.claim(key, GRACE, late, late + TimeDelta::minutes(5)).await.unwrap();
        assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
        if delete {
            self.storage.delete(key).await.unwrap();
        }
    }

    async fn row(&self, version: &str) -> Option<String> {
        let packages = self.store.packages();
        let package = packages.package(self.repo, "p", NameMatch::Exact).await.unwrap()?;
        packages
            .version(package.id, version)
            .await
            .unwrap()
            .map(|v| v.tarball_path)
    }
}

fn artifact(repository: i64, version: &str, body: &'static [u8]) -> Artifact<'static> {
    Artifact {
        repository,
        package: "p",
        match_name: NameMatch::Exact,
        description: None,
        readme: None,
        version: Box::leak(version.to_string().into_boxed_str()),
        metadata_json: "{}",
        checksum_sha1: None,
        checksum_sha256: None,
        integrity: None,
        filename: "p.tgz",
        dist_tags: &[],
        bytes: Bytes::from_static(body),
    }
}

fn release<'a>(repo: i64, version: &'a str, pins: &'a [PinToken]) -> NewRelease<'a> {
    NewRelease {
        repository: repo,
        package: "p",
        match_name: NameMatch::Exact,
        description: None,
        readme: None,
        version,
        metadata_json: "{}",
        checksum_sha1: None,
        checksum_sha256: None,
        integrity: None,
        size: 4,
        tarball_path: &pins[0].physical_key,
        dist_tags: &[],
        dependencies: &[],
        pins,
        now: Utc::now(),
    }
}

#[tokio::test]
async fn conflict_loser_enqueues_and_deletes_nothing() {
    let fx = Fx::new().await;
    let winner = fx.publisher().run(artifact(fx.repo, "1.0.0", b"same"), Utc::now()).await.unwrap();
    let refused = fx
        .publisher()
        .run(artifact(fx.repo, "1.0.0", b"same"), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(refused, PublishError::Store(StoreError::Conflict)), "{refused:?}");
    assert!(fx.storage.deleted().is_empty(), "nothing deleted inline");
    assert!(fx.storage.contains(&winner.version.tarball_path));
    assert_eq!(
        fx.store.candidates(),
        vec![winner.version.tarball_path.clone()],
        "identical bytes reuse the referenced generation, which the loser enqueues"
    );

    let report = crate::app::reclaim::ReclaimOrphans::new(
        fx.store.reclaim(),
        fx.store.referenced(),
        Arc::new(fx.storage.clone()),
        crate::app::reclaim::ReclaimPolicy::default(),
    )
    .run(Utc::now() + TimeDelta::hours(3))
    .await;
    assert_eq!((report.reclaimed, report.referenced), (0, 1), "the claim finds the winner's row");
    assert!(fx.storage.contains(&winner.version.tarball_path), "the winner's bytes stay");
    let other = fx
        .publisher()
        .run(artifact(fx.repo, "1.0.0", b"different"), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(other, PublishError::Store(StoreError::Conflict)));
    let queued = fx.store.candidates();
    assert_eq!(queued.len(), 1, "a different body is a fresh generation, enqueued");
    assert_ne!(queued[0], winner.version.tarball_path);
}

#[tokio::test]
async fn slow_placer_with_expired_pin_is_superseded_and_rewrites() {
    let fx = Fx::new().await;
    let attempts = AtomicUsize::new(0);
    let entries = [Entry {
        logical_key: fx.logical(),
        source: Source::Bytes(Bytes::from_static(b"body")),
    }];
    let packages = fx.store.packages();
    let landed = fx
        .placer()
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, attempts) = (&fx, packages.clone(), &attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        fx.reclaim_now(&pins[0].physical_key, true).await;
                    }
                    packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await
                }
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "superseded once, then committed");
    assert_eq!(
        fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(),
        b"body",
        "the row serves bytes"
    );
}

#[tokio::test]
async fn stale_reclaimer_waking_after_takeover_deletes_only_its_generation() {
    let fx = Fx::new().await;
    let entries = [Entry {
        logical_key: fx.logical(),
        source: Source::Bytes(Bytes::from_static(b"body")),
    }];
    let first = AtomicUsize::new(0);
    let stale_key = std::sync::Mutex::new(String::new());
    let packages = fx.store.packages();
    let landed = fx
        .placer()
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, first, stale_key) = (&fx, packages.clone(), &first, &stale_key);
                async move {
                    if first.fetch_add(1, Ordering::SeqCst) == 0 {
                        fx.reclaim_now(&pins[0].physical_key, false).await;
                        *stale_key.lock().unwrap() = pins[0].physical_key.clone();
                    }
                    packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await
                }
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let stale = stale_key.lock().unwrap().clone();
    assert_ne!(landed.version.tarball_path, stale, "a claimed generation is never reused");
    fx.storage.delete(&stale).await.unwrap();
    assert_eq!(
        fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(),
        b"body",
        "the late delete reached only garbage"
    );
}

#[tokio::test]
async fn placement_during_claim_takes_a_fresh_generation() {
    let fx = Fx::new().await;
    let first = fx.publisher().run(artifact(fx.repo, "1.0.0", b"same"), Utc::now()).await.unwrap();
    let old = first.version.tarball_path.clone();
    fx.store
        .packages()
        .delete_version(first.version.id, Utc::now() - TimeDelta::hours(2))
        .await
        .unwrap();
    let late = Utc::now() + TimeDelta::hours(3);
    let claim = fx
        .store
        .reclaim()
        .claim(&old, GRACE, late, late + TimeDelta::minutes(5))
        .await
        .unwrap();
    assert!(matches!(claim, Claim::Claimed(_)));
    let again = fx.publisher().run(artifact(fx.repo, "1.0.0", b"same"), Utc::now()).await.unwrap();
    assert_ne!(again.version.tarball_path, old);
}

#[tokio::test]
async fn delete_version_enqueues_and_deletes_nothing_under_an_identical_publish() {
    let fx = Fx::new().await;
    let first = fx.publisher().run(artifact(fx.repo, "1.0.0", b"same"), Utc::now()).await.unwrap();
    let key = first.version.tarball_path.clone();
    let packages = fx.store.packages();
    let entries = [Entry {
        logical_key: key.rsplit_once('~').unwrap().0.to_string(),
        source: Source::Bytes(Bytes::from_static(b"same")),
    }];
    let landed = fx
        .placer()
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, first) = (&fx, packages.clone(), &first);
                async move {
                    assert_eq!(pins[0].physical_key, first.version.tarball_path, "a referenced generation is reused");
                    packages.delete_version(first.version.id, Utc::now()).await.unwrap();
                    packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await
                }
            },
            Utc::now(),
        )
        .await;
    let landed = match landed {
        Ok(release) => release,
        Err(e) => panic!("{e}"),
    };
    assert!(fx.storage.deleted().is_empty(), "delete_version deleted nothing");
    assert_eq!(fx.store.candidates(), vec![key.clone()]);
    let report = crate::app::reclaim::ReclaimOrphans::new(
        fx.store.reclaim(),
        fx.store.referenced(),
        Arc::new(fx.storage.clone()),
        crate::app::reclaim::ReclaimPolicy::default(),
    )
    .run(Utc::now() + TimeDelta::hours(3))
    .await;
    assert_eq!(report.referenced, 1, "the claim finds the new row");
    assert_eq!(fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(), b"same");
    assert_eq!(fx.row("1.0.0").await, Some(key));
}

#[tokio::test]
async fn delete_version_under_an_identical_publish_whose_pin_is_pruned_rewrites_fresh() {
    let fx = Fx::new().await;
    let first = fx.publisher().run(artifact(fx.repo, "1.0.0", b"same"), Utc::now()).await.unwrap();
    let reused = first.version.tarball_path.clone();
    let packages = fx.store.packages();
    let entries = [Entry {
        logical_key: reused.rsplit_once('~').unwrap().0.to_string(),
        source: Source::Bytes(Bytes::from_static(b"same")),
    }];
    let attempts = AtomicUsize::new(0);
    let landed = fx
        .placer()
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, first, attempts) = (&fx, packages.clone(), &first, &attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        assert_eq!(pins[0].physical_key, first.version.tarball_path, "G is reused");
                        packages.delete_version(first.version.id, Utc::now()).await.unwrap();
                        let now = Utc::now();
                        let claim = fx
                            .store
                            .reclaim()
                            .claim(&pins[0].physical_key, Duration::ZERO, now, now + TimeDelta::minutes(5))
                            .await
                            .unwrap();
                        assert_eq!(claim, Claim::Pinned, "the publisher's pin protects G");
                        let report = crate::app::reclaim::ReclaimOrphans::new(
                            fx.store.reclaim(),
                            fx.store.referenced(),
                            Arc::new(fx.storage.clone()),
                            crate::app::reclaim::ReclaimPolicy::default(),
                        )
                        .run(Utc::now() + TimeDelta::hours(3))
                        .await;
                        assert_eq!((report.pruned_pins, report.reclaimed), (1, 1), "{report:?}");
                        assert!(!fx.storage.contains(&pins[0].physical_key), "G is gone");
                    }
                    let answer = packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await;
                    if attempts.load(Ordering::SeqCst) == 1 {
                        assert!(matches!(answer, Err(StoreError::Superseded(_))), "{answer:?}");
                    }
                    answer
                }
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "superseded once, then committed");
    assert_ne!(landed.version.tarball_path, reused, "a fresh generation");
    assert_eq!(fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(), b"same");
    assert_eq!(fx.row("1.0.0").await, Some(landed.version.tarball_path));
}

#[tokio::test]
async fn draft_placement_relocates_under_pin_and_enqueues_on_superseded() {
    let fx = Fx::new().await;
    let placer = fx.placer();
    let draft = placer.draft(&fx.prefix, Utc::now()).await.unwrap();
    fx.storage.put(&draft, Bytes::from_static(b"drafted")).await.unwrap();
    let entries = [Entry {
        logical_key: fx.logical(),
        source: Source::Draft(draft.clone()),
    }];
    let packages = fx.store.packages();
    let attempts = AtomicUsize::new(0);
    let refused = placer
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, attempts) = (&fx, packages.clone(), &attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    fx.reclaim_now(&pins[0].physical_key, true).await;
                    packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await
                }
            },
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(matches!(refused, PlaceError::Unavailable), "{refused:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "a moved draft is not replayed from nothing");
    assert!(!fx.storage.contains(&draft), "the draft was moved into place");
    assert!(fx.row("1.0.0").await.is_none(), "nothing recorded");
    assert_eq!(
        fx.store.candidates().len(),
        2,
        "the claimed generation, and the fresh one this call pinned and never wrote"
    );

    let draft = placer.draft(&fx.prefix, Utc::now()).await.unwrap();
    fx.storage.put(&draft, Bytes::from_static(b"drafted")).await.unwrap();
    let entries = [Entry {
        logical_key: fx.logical(),
        source: Source::Draft(draft),
    }];
    let once = AtomicUsize::new(0);
    let landed = placer
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, once) = (&fx, packages.clone(), &once);
                async move {
                    if once.fetch_add(1, Ordering::SeqCst) == 0 {
                        fx.reclaim_now(&pins[0].physical_key, false).await;
                    }
                    packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await
                }
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(),
        b"drafted",
        "copied back from the revoked generation, which was still whole"
    );
}

#[tokio::test]
async fn commit_after_retire_is_superseded_not_fk_error() {
    let fx = Fx::new().await;
    let entries = [Entry {
        logical_key: fx.logical(),
        source: Source::Bytes(Bytes::from_static(b"body")),
    }];
    let packages = fx.store.packages();
    let repos = fx.store.repositories();
    let refused = fx
        .placer()
        .place_shared(
            &fx.prefix,
            &entries,
            |pins| {
                let (fx, packages, repos) = (&fx, packages.clone(), repos.clone());
                async move {
                    if repos.by_name("r").await.unwrap().is_some() {
                        repos.retire("r", Utc::now()).await.unwrap();
                    }
                    let answer = packages.publish_version(&release(fx.repo, "1.0.0", &pins)).await;
                    assert!(matches!(answer, Err(StoreError::Superseded(_))), "{answer:?}");
                    answer
                }
            },
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(matches!(refused, PlaceError::Retired), "{refused:?}");
}
