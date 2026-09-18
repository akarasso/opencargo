use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::TimeDelta;

use super::*;
use crate::domain::{DistTag, Format, Package, RepoKind, RepoSpec, Version, Visibility};
use crate::error::StoreError;
use crate::ports::packages::{Promotion, StalePrerelease};
use crate::ports::reclaim::Claim;
use crate::testing::fakes::FakeDb;
use crate::storage::StorageBackend;
use crate::testing::storage::MemStorage;

const GRACE: Duration = Duration::from_secs(3600);

/// A `PackageStore` whose first commit finds its pin claimed and its
/// generation deleted by a reclaimer: the interleaving hook of NuGet 2.4.
struct ReclaimedOnce {
    inner: Arc<dyn PackageStore>,
    fakes: FakeDb,
    storage: MemStorage,
    commits: AtomicUsize,
}

#[async_trait]
impl PackageStore for ReclaimedOnce {
    async fn package(&self, r: i64, n: &str, m: NameMatch) -> Result<Option<Package>, StoreError> {
        self.inner.package(r, n, m).await
    }
    async fn anywhere(&self, n: &str, p: bool) -> Result<Option<Package>, StoreError> {
        self.inner.anywhere(n, p).await
    }
    async fn versions(&self, p: i64) -> Result<Vec<Version>, StoreError> {
        self.inner.versions(p).await
    }
    async fn version(&self, p: i64, v: &str) -> Result<Option<Version>, StoreError> {
        self.inner.version(p, v).await
    }
    async fn dist_tags(&self, p: i64) -> Result<Vec<DistTag>, StoreError> {
        self.inner.dist_tags(p).await
    }
    async fn publish_version(&self, release: &NewRelease<'_>) -> Result<Release, StoreError> {
        if self.commits.fetch_add(1, Ordering::SeqCst) == 0 {
            let key = release.pins[0].physical_key.clone();
            let reclaim = self.fakes.reclaim();
            let late = Utc::now() + TimeDelta::hours(3);
            reclaim.enqueue(std::slice::from_ref(&key), Utc::now() - TimeDelta::hours(2)).await?;
            let claim = reclaim.claim(&key, GRACE, late, late + TimeDelta::minutes(5)).await?;
            assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
            self.storage.vanish(&key);
        }
        self.inner.publish_version(release).await
    }
    async fn promote_metadata(&self, p: &Promotion<'_>) -> Result<Version, StoreError> {
        self.inner.promote_metadata(p).await
    }
    async fn set_dist_tag(&self, p: i64, t: &str, v: i64) -> Result<(), StoreError> {
        self.inner.set_dist_tag(p, t, v).await
    }
    async fn clear_dist_tag(&self, p: i64, t: &str) -> Result<(), StoreError> {
        self.inner.clear_dist_tag(p, t).await
    }
    async fn set_readme(&self, p: i64, r: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.set_readme(p, r, now).await
    }
    async fn set_metadata(&self, v: i64, m: &str) -> Result<(), StoreError> {
        self.inner.set_metadata(v, m).await
    }
    async fn set_yanked(&self, v: i64, y: bool) -> Result<(), StoreError> {
        self.inner.set_yanked(v, y).await
    }
    async fn stale_prereleases(
        &self,
        o: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<StalePrerelease>, StoreError> {
        self.inner.stale_prereleases(o, now).await
    }
    async fn delete_version(&self, v: i64, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.delete_version(v, now).await
    }
    async fn record_download(&self, v: i64) -> Result<(), StoreError> {
        self.inner.record_download(v).await
    }
}

struct Fx {
    fakes: FakeDb,
    storage: MemStorage,
    repo: i64,
}

impl Fx {
    async fn new() -> Self {
        let db = FakeDb::new();
        let repo = db
            .repositories()
            .create(
                &RepoSpec {
                    name: "nuget",
                    kind: RepoKind::Hosted,
                    format: Format::Nuget,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                },
                Utc::now(),
            )
            .await
            .unwrap();
        Self {
            fakes: db,
            storage: MemStorage::new(),
            repo: repo.id,
        }
    }

    fn with_packages(&self, packages: Arc<dyn PackageStore>) -> PublishNugetPackage {
        PublishNugetPackage::new(
            packages,
            self.fakes.repositories(),
            self.fakes.dependencies(),
            Arc::new(Placer::new(self.fakes.reclaim(), Arc::new(self.storage.clone()))),
            crate::registry::rules::rules_of(crate::domain::Format::Nuget).unwrap(),
        )
    }

    fn publisher(&self) -> PublishNugetPackage {
        self.with_packages(self.fakes.packages())
    }

    fn push<'a>(&self, id: &'a str, version: &'a str, body: &'static [u8]) -> NugetPush<'a> {
        NugetPush {
            repository: self.repo,
            id,
            version,
            description: Some("d"),
            metadata_json: "{}",
            dependencies: &[],
            spool: Bytes::from_static(body),
        }
    }
}

#[tokio::test]
async fn a_push_lands_under_the_normalized_id_and_version() {
    let fx = Fx::new().await;
    let landed = fx
        .publisher()
        .run(fx.push("My.Lib", "01.0.0.0", b"pkg"), Utc::now())
        .await
        .unwrap();
    assert_eq!(landed.package.name, "my.lib");
    assert_eq!(landed.version.version, "1.0.0");
    assert!(landed.version.tarball_path.contains("/my.lib/"));
    assert!(landed.version.tarball_path.contains("my.lib.1.0.0.nupkg~"));
    assert!(fx.storage.contains(&landed.version.tarball_path));
}

#[tokio::test]
async fn two_spellings_of_one_version_are_a_conflict() {
    let fx = Fx::new().await;
    fx.publisher().run(fx.push("A", "1.0", b"one"), Utc::now()).await.unwrap();
    for spelling in ["1.0.0.0", "1.0.0+meta", "1.0.0"] {
        let refused = fx
            .publisher()
            .run(fx.push("a", spelling, b"two"), Utc::now())
            .await
            .unwrap_err();
        assert!(
            matches!(refused, PushError::Publish(PublishError::Store(StoreError::Conflict))),
            "{spelling}: {refused:?}"
        );
    }
}

#[tokio::test]
async fn an_invalid_id_or_version_writes_nothing() {
    let fx = Fx::new().await;
    for (id, version) in [("../x", "1.0.0"), ("a", "1.0/..")] {
        let refused = fx.publisher().run(fx.push(id, version, b"x"), Utc::now()).await.unwrap_err();
        assert!(matches!(refused, PushError::Invalid(_)), "{refused:?}");
    }
    assert!(fx.storage.keys().is_empty());
}

#[tokio::test]
async fn conflict_loser_enqueues_and_deletes_nothing() {
    let fx = Fx::new().await;
    let winner = fx.publisher().run(fx.push("a", "1.0.0", b"one"), Utc::now()).await.unwrap();
    fx.publisher().run(fx.push("a", "1.0.0", b"two"), Utc::now()).await.unwrap_err();
    assert!(fx.storage.deleted().is_empty(), "nothing deleted inline");
    let queued = fx.fakes.candidates();
    assert_eq!(queued.len(), 1);
    assert_ne!(queued[0], winner.version.tarball_path);
    assert!(fx.storage.contains(&winner.version.tarball_path));
}

#[tokio::test]
async fn superseded_push_replaces_and_commits() {
    let fx = Fx::new().await;
    let packages = Arc::new(ReclaimedOnce {
        inner: fx.fakes.packages(),
        fakes: fx.fakes.clone(),
        storage: fx.storage.clone(),
        commits: AtomicUsize::new(0),
    });
    let landed = fx
        .with_packages(packages.clone())
        .run(fx.push("a", "1.0.0", b"spooled"), Utc::now())
        .await
        .unwrap();
    assert_eq!(packages.commits.load(Ordering::SeqCst), 2, "superseded once, then committed");
    assert_eq!(
        fx.storage.get(&landed.version.tarball_path).await.unwrap().as_ref(),
        b"spooled",
        "the new generation was written again from the spool"
    );
    assert!(fx.storage.deleted().is_empty(), "the push deletes nothing");
}

#[tokio::test]
async fn push_leaves_no_part_file() {
    let fx = Fx::new().await;
    let landed = fx.publisher().run(fx.push("a", "1.0.0", b"pkg"), Utc::now()).await.unwrap();
    assert_eq!(fx.storage.keys(), vec![landed.version.tarball_path]);
}
