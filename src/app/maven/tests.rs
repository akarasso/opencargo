use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};

use super::deposit::{Body, DepositError, Deposited, MavenDeposits, Target};
use super::versions::{Announcer, MavenVersions};
use crate::app::place::Placer;
use crate::app::publish_tail::Published;
use crate::domain::{layout, Format, RepoKind, RepoSpec, Visibility};
use crate::error::{AppError, StoreError};
use crate::ports::maven::{
    Changed, ClientMetadata, Counter, MavenFileStore, PendingUnit, SumAlgorithm, Unit, UnitChange,
    UnitKey, UnitView, Unversioned,
};
use crate::ports::packages::NameMatch;
use crate::ports::reclaim::Claim;
use crate::registry::maven::hosted;
use crate::registry::maven::path::Gav;
use crate::storage::StorageBackend;
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

const GA: &str = "org.example:lib";
const PACKAGE: &str = "org/example/lib";
const GRACE: std::time::Duration = std::time::Duration::from_secs(3600);

#[derive(Default)]
struct Recorder(Mutex<Vec<(String, String)>>);

#[async_trait]
impl Announcer for Recorder {
    async fn announce(&self, done: &Published<'_>, _now: DateTime<Utc>) {
        self.0
            .lock()
            .unwrap()
            .push((done.package.to_string(), done.version.to_string()));
    }
}

/// What a reclaimer that wins a claim on a placement's key does, fired once
/// from inside that placement's commit: revoke, and delete when asked.
struct Hooked {
    inner: Arc<dyn MavenFileStore>,
    store: FakeDb,
    storage: MemStorage,
    delete: bool,
    armed: AtomicBool,
    during: Mutex<Option<Box<dyn Fn() + Send>>>,
    after_read: Mutex<Option<futures_util::future::BoxFuture<'static, ()>>>,
}

#[async_trait]
impl MavenFileStore for Hooked {
    async fn unit(&self, key: &UnitKey<'_>) -> Result<Option<Unit>, StoreError> {
        self.inner.unit(key).await
    }

    async fn change(&self, change: &UnitChange<'_>) -> Result<Changed, StoreError> {
        if let Some(file) = &change.file {
            if self.armed.swap(false, Ordering::SeqCst) {
                let reclaim = self.store.reclaim();
                let key = file.physical_key.to_string();
                let late = Utc::now() + TimeDelta::hours(3);
                reclaim
                    .enqueue(std::slice::from_ref(&key), Utc::now() - TimeDelta::hours(2))
                    .await
                    .unwrap();
                let claim = reclaim.claim(&key, GRACE, late, late + TimeDelta::minutes(5)).await.unwrap();
                assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
                if self.delete {
                    self.storage.vanish(&key);
                }
            }
        }
        if let Some(during) = self.during.lock().unwrap().as_ref() {
            during();
        }
        self.inner.change(change).await
    }

    async fn refuse(&self, key: &UnitKey<'_>, revision: i64, scopes: &[String], now: DateTime<Utc>) -> Result<Changed, StoreError> {
        self.inner.refuse(key, revision, scopes, now).await
    }

    async fn artifact(&self, repository: i64, ga: &str) -> Result<Vec<UnitView>, StoreError> {
        let units = self.inner.artifact(repository, ga).await;
        let after = self.after_read.lock().unwrap().take();
        if let Some(after) = after {
            after.await;
        }
        units
    }

    async fn counter(&self, repository: i64, scope: &str) -> Result<Counter, StoreError> {
        self.inner.counter(repository, scope).await
    }

    async fn record_client_metadata(&self, m: &ClientMetadata, scopes: &[String], now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.record_client_metadata(m, scopes, now).await
    }

    async fn client_metadata(&self, repository: i64, dir: &str) -> Result<Option<ClientMetadata>, StoreError> {
        self.inner.client_metadata(repository, dir).await
    }

    async fn pending(&self, before: DateTime<Utc>, limit: u32) -> Result<Vec<PendingUnit>, StoreError> {
        self.inner.pending(before, limit).await
    }

    async fn unversioned(&self, after: Option<&Unversioned>, limit: u32) -> Result<Vec<Unversioned>, StoreError> {
        self.inner.unversioned(after, limit).await
    }

    async fn mark_versioned(&self, repository: i64, ga: &str, version: &str) -> Result<(), StoreError> {
        self.inner.mark_versioned(repository, ga, version).await
    }
}

struct Fx {
    store: FakeDb,
    storage: MemStorage,
    repo: i64,
    prefix: String,
    announced: Arc<Recorder>,
    maven: Arc<dyn MavenFileStore>,
}

impl Fx {
    async fn new() -> Self {
        let db = FakeDb::new();
        let repo = db
            .repositories()
            .create(
                &RepoSpec {
                    name: "m",
                    kind: RepoKind::Hosted,
                    format: Format::Maven,
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
            maven: db.maven(),
            storage: MemStorage::new(),
            prefix: layout::incarnation_prefix(&incarnation),
            announced: Arc::new(Recorder::default()),
            repo: repo.id,
            store: db,
        }
    }

    fn hooked(mut self, delete: bool) -> (Self, Arc<Hooked>) {
        let hooked = Arc::new(Hooked {
            inner: self.store.maven(),
            store: self.store.clone(),
            storage: self.storage.clone(),
            delete,
            armed: AtomicBool::new(false),
            during: Mutex::new(None),
            after_read: Mutex::new(None),
        });
        self.maven = hooked.clone();
        (self, hooked)
    }

    fn versions(&self) -> Arc<MavenVersions> {
        Arc::new(MavenVersions::new(
            self.maven.clone(),
            self.store.packages(),
            self.store.repositories(),
            Arc::new(self.storage.clone()),
            self.announced.clone(),
            crate::registry::maven::pom::metadata_json,
        ))
    }

    fn deposits(&self) -> MavenDeposits {
        let storage: Arc<dyn StorageBackend> = Arc::new(self.storage.clone());
        MavenDeposits::new(
            self.store.repositories(),
            storage.clone(),
            Arc::new(Placer::new(self.store.reclaim(), storage)),
            self.versions(),
        )
    }

    fn reconciler(&self) -> super::reconcile::MavenReconcile {
        self.reconciler_of(100)
    }

    fn reconciler_of(&self, limit: u32) -> super::reconcile::MavenReconcile {
        super::reconcile::MavenReconcile::new(
            self.versions(),
            TimeDelta::minutes(10),
            limit,
            hosted::scopes_of_unit,
        )
    }

    fn announcements(&self) -> usize {
        self.announced.0.lock().unwrap().len()
    }

    async fn put(&self, version: &str, filename: &str, who: &str, body: &'static [u8]) -> Result<Deposited, DepositError> {
        let build = build_of(version, filename);
        let scopes = scopes(version);
        let target = Target {
            unit: UnitKey {
                repository: self.repo,
                ga: GA,
                version,
                build: &build,
            },
            package_path: PACKAGE,
            filename,
            principal: who,
            scopes: &scopes,
        };
        let body: Body = Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from_static(body))]));
        self.deposits().file(target, body, Utc::now()).await
    }

    async fn sum(&self, version: &str, filename: &str, who: &str, algorithm: SumAlgorithm, value: &str) -> Result<Deposited, DepositError> {
        let build = build_of(version, filename);
        let scopes = scopes(version);
        let target = Target {
            unit: UnitKey {
                repository: self.repo,
                ga: GA,
                version,
                build: &build,
            },
            package_path: PACKAGE,
            filename,
            principal: who,
            scopes: &scopes,
        };
        self.deposits().sum(target, algorithm, value, Utc::now()).await
    }

    async fn unit(&self, version: &str, build: &str) -> Option<Unit> {
        self.store
            .maven()
            .unit(&UnitKey {
                repository: self.repo,
                ga: GA,
                version,
                build,
            })
            .await
            .unwrap()
    }

    async fn snapshot(&self, version: &str) -> Option<hosted::Rendered> {
        hosted::snapshot_metadata(self.store.maven().as_ref(), self.store.packages().as_ref(), self.repo, &gav(version))
            .await
            .unwrap()
    }

    async fn version_rows(&self) -> usize {
        let packages = self.store.packages();
        match packages.package(self.repo, GA, NameMatch::Exact).await.unwrap() {
            Some(p) => packages.versions(p.id).await.unwrap().len(),
            None => 0,
        }
    }
}

fn gav(version: &str) -> Gav {
    Gav {
        group: "org.example".into(),
        artifact: "lib".into(),
        version: version.into(),
    }
}

fn scopes(version: &str) -> Vec<String> {
    hosted::scopes_of(&gav(version))
}

fn build_of(version: &str, filename: &str) -> String {
    let base = version.trim_end_matches("-SNAPSHOT");
    if base == version {
        return String::new();
    }
    let rest = filename.strip_prefix(&format!("lib-{base}-")).unwrap_or_default();
    crate::registry::maven::path::parse_build(rest)
        .map(|(_, _, used)| rest[..used].to_string())
        .unwrap_or_default()
}

fn digests(body: &[u8]) -> crate::ports::maven::Digests {
    super::deposit::digests_of(body)
}

#[tokio::test]
async fn maven_deposit_goes_draft_pin_relocate() {
    let fx = Fx::new().await;
    let done = fx.put("1.0", "lib-1.0.jar", "alice", b"jar-bytes").await.unwrap();
    assert_eq!(done, Deposited::Stored { revealed: false });

    let unit = fx.unit("1.0", "").await.unwrap();
    let file = unit.file("lib-1.0.jar").unwrap();
    let logical = layout::hosted_key(&fx.prefix, PACKAGE, &digests(b"jar-bytes").sha256, "lib-1.0.jar");
    assert_eq!(layout::logical_key(&file.physical_key), logical, "pinned under the digest the draft computed");
    assert_eq!(fx.storage.keys(), vec![file.physical_key.clone()], "the draft was relocated, nothing else is left");
    assert_eq!(fx.storage.get(&file.physical_key).await.unwrap().as_ref(), b"jar-bytes");
    let drafts = format!("{}/_drafts/", fx.prefix);
    assert!(fx.storage.deleted().iter().all(|k| k.starts_with(&drafts)), "only the draft's own writer deletes");
    assert!(fx.store.candidates().is_empty());
}

#[tokio::test]
async fn maven_superseded_after_body_consumed_copies_or_503() {
    let (fx, hooked) = Fx::new().await.hooked(false);
    hooked.armed.store(true, Ordering::SeqCst);
    let done = fx.put("1.0", "lib-1.0.jar", "alice", b"whole").await.unwrap();
    assert_eq!(done, Deposited::Stored { revealed: false });
    let file = fx.unit("1.0", "").await.unwrap().file("lib-1.0.jar").unwrap().clone();
    assert_eq!(
        fx.storage.get(&file.physical_key).await.unwrap().as_ref(),
        b"whole",
        "copied from the revoked generation, which was still whole"
    );

    let (fx, hooked) = Fx::new().await.hooked(true);
    hooked.armed.store(true, Ordering::SeqCst);
    let refused = fx.put("1.0", "lib-1.0.jar", "alice", b"gone").await.unwrap_err();
    assert!(matches!(refused, DepositError::Unavailable), "{refused:?}");
    assert!(matches!(AppError::from(refused), AppError::ServiceUnavailable(_)));
    assert!(fx.unit("1.0", "").await.is_none(), "nothing recorded");
    assert!(fx.storage.deleted().iter().all(|k| k.contains("/_drafts/")), "nothing deleted but the draft");
    assert_eq!(fx.store.candidates().len(), 2, "the claimed generation and the fresh one never written");
}

#[tokio::test]
async fn snapshot_build_announced_only_after_pom_commit() {
    let (fx, hooked) = Fx::new().await.hooked(false);
    let v = "1.0-SNAPSHOT";
    fx.put(v, "lib-1.0-20260918.120000-1.jar", "ci", b"jar1").await.unwrap();
    fx.put(v, "lib-1.0-20260918.120000-1.pom", "ci", b"<project/>").await.unwrap();
    let first = fx.snapshot(v).await.unwrap();
    assert!(String::from_utf8_lossy(&first.body).contains("<value>1.0-20260918.120000-1</value>"));

    fx.put(v, "lib-1.0-20260918.130000-2.jar", "ci", b"jar2").await.unwrap();
    let body = String::from_utf8_lossy(&fx.snapshot(v).await.unwrap().body).to_string();
    assert!(body.contains("<buildNumber>1</buildNumber>"), "a jar without its POM is not announced");
    assert!(!body.contains("130000-2"), "{body}");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (db, repo, seen_in) = (fx.store.clone(), fx.repo, seen.clone());
    *hooked.during.lock().unwrap() = Some(Box::new(move || {
        use futures_util::FutureExt;
        let (maven, packages) = (db.maven(), db.packages());
        let doc = hosted::snapshot_metadata(maven.as_ref(), packages.as_ref(), repo, &gav(v))
            .now_or_never()
            .expect("the fakes answer at once");
        seen_in.lock().unwrap().push(String::from_utf8_lossy(&doc.unwrap().unwrap().body).to_string());
    }));
    fx.put(v, "lib-1.0-20260918.130000-2.pom", "ci", b"<project/>").await.unwrap();
    *hooked.during.lock().unwrap() = None;
    let in_flight = seen.lock().unwrap().clone();
    assert!(!in_flight.is_empty());
    assert!(in_flight.iter().all(|b| b.contains("<buildNumber>1</buildNumber>")), "the POM in flight announces nothing");

    let after = String::from_utf8_lossy(&fx.snapshot(v).await.unwrap().body).to_string();
    assert!(after.contains("<buildNumber>2</buildNumber>"));
    assert_eq!(after.matches("<value>1.0-20260918.130000-2</value>").count(), 2, "pom and jar of one build");
    assert!(!after.contains("120000-1"), "no entry from an older build: {after}");
}

#[tokio::test]
async fn a_commit_between_body_and_validator_reads_never_pairs_an_old_body_with_a_new_etag() {
    let (fx, hooked) = Fx::new().await.hooked(false);
    let fx = Arc::new(fx);
    let v = "1.0-SNAPSHOT";
    fx.put(v, "lib-1.0-20260918.120000-1.jar", "ci", b"jar1").await.unwrap();
    fx.put(v, "lib-1.0-20260918.120000-1.pom", "ci", b"<project/>").await.unwrap();
    fx.put(v, "lib-1.0-20260918.130000-2.jar", "ci", b"jar2").await.unwrap();

    let racer = fx.clone();
    *hooked.after_read.lock().unwrap() = Some(Box::pin(async move {
        racer.put(v, "lib-1.0-20260918.130000-2.pom", "ci", b"<project/>").await.unwrap();
    }));
    let raced = hosted::snapshot_metadata(hooked.as_ref(), fx.store.packages().as_ref(), fx.repo, &gav(v))
        .await
        .unwrap()
        .unwrap();
    assert!(hooked.after_read.lock().unwrap().is_none(), "the commit fired between the reads");
    assert!(String::from_utf8_lossy(&raced.body).contains("<buildNumber>1</buildNumber>"));

    let current = fx.snapshot(v).await.unwrap();
    assert_ne!(raced.body, current.body);
    assert_ne!(raced.etag, current.etag, "If-None-Match on the raced ETag must not answer 304");
}

#[tokio::test]
async fn third_party_checksum_marks_value_contested_not_400() {
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap();
    let refused = fx.sum("1.0", "lib-1.0.jar", "bob", SumAlgorithm::Sha1, &"0".repeat(40)).await.unwrap_err();
    assert!(matches!(refused, DepositError::Contested), "{refused:?}");
    let unit = fx.unit("1.0", "").await.unwrap();
    assert!(unit.contested);
    assert!(unit.declarations.is_empty(), "never kept");

    let right = digests(b"jar").sha1;
    assert_eq!(
        fx.sum("1.0", "lib-1.0.jar", "alice", SumAlgorithm::Sha1, &right).await.unwrap(),
        Deposited::Stored { revealed: false },
        "the depositor never gets a 400 because of a third party"
    );
    let done = fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    assert_eq!(done, Deposited::Stored { revealed: true }, "a contested unit is revealed by its depositor's POM");
}

#[tokio::test]
async fn a_third_party_jar_without_pom_contests_and_is_never_promoted() {
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap();
    let refused = fx.put("1.0", "lib-1.0.jar", "bob", b"evil").await.unwrap_err();
    assert!(matches!(refused, DepositError::Contested), "{refused:?}");
    assert!(matches!(AppError::from(refused), AppError::Conflict(_)));
    let unit = fx.unit("1.0", "").await.unwrap();
    assert_eq!(unit.file("lib-1.0.jar").unwrap().digests, digests(b"jar"), "the first bytes stay");
    assert!(unit.contested);
    let later = unit.created_at + TimeDelta::days(1);
    assert!(!super::rules::promotable(&unit, later, TimeDelta::minutes(10)));
    assert_eq!(fx.put("1.0", "lib-1.0.jar", "bob", b"jar").await.unwrap(), Deposited::Unchanged, "same bytes: 200");
}

#[tokio::test]
async fn a_sum_before_its_artifact_refuses_different_bytes() {
    for algorithm in SumAlgorithm::ALL {
        let fx = Fx::new().await;
        let right = digests(b"jar").get(algorithm).to_string();
        fx.sum("1.0", "lib-1.0.jar", "alice", algorithm, &right).await.unwrap();
        let refused = fx.put("1.0", "lib-1.0.jar", "alice", b"other").await.unwrap_err();
        assert!(matches!(refused, DepositError::Mismatch(a) if a == algorithm), "{refused:?}");
        assert!(matches!(AppError::from(refused), AppError::BadRequest(_)));
        assert!(fx.unit("1.0", "").await.unwrap().files.is_empty(), "nothing it carried was kept");
        assert!(fx.storage.keys().is_empty());
        fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap();
    }
}

#[tokio::test]
async fn the_depositor_replaces_pending_bytes_and_their_declarations() {
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.jar", "alice", b"v1").await.unwrap();
    fx.sum("1.0", "lib-1.0.jar", "alice", SumAlgorithm::Md5, &digests(b"v1").md5).await.unwrap();
    let old = fx.unit("1.0", "").await.unwrap().file("lib-1.0.jar").unwrap().physical_key.clone();
    fx.put("1.0", "lib-1.0.jar", "alice", b"v2").await.unwrap();
    let unit = fx.unit("1.0", "").await.unwrap();
    assert_eq!(unit.file("lib-1.0.jar").unwrap().digests, digests(b"v2"));
    assert!(unit.declarations.is_empty());
    assert_eq!(fx.store.candidates(), vec![old.clone()], "enqueued");
    assert!(fx.storage.contains(&old), "never deleted inline");
    fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    let refused = fx.put("1.0", "lib-1.0.jar", "alice", b"v3").await.unwrap_err();
    assert!(matches!(refused, DepositError::Refused(_)), "visible is immutable: {refused:?}");
}

#[tokio::test]
async fn a_signature_is_stored_as_sent() {
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    fx.put("1.0", "lib-1.0.pom.asc", "alice", b"not even a signature").await.unwrap();
    let file = fx.unit("1.0", "").await.unwrap().file("lib-1.0.pom.asc").unwrap().clone();
    assert_eq!(fx.storage.get(&file.physical_key).await.unwrap().as_ref(), b"not even a signature");
}

#[tokio::test]
async fn a_new_build_moves_the_counter_and_not_the_version_stamp() {
    let fx = Fx::new().await;
    let v = "1.0-SNAPSHOT";
    let scope = hosted::scope_snapshot(GA, v);
    let counter = || async { fx.store.maven().counter(fx.repo, &scope).await.unwrap().value };
    assert_eq!(fx.version_rows().await, 0);
    fx.put(v, "lib-1.0-20260918.120000-1.pom", "ci", b"<project/>").await.unwrap();
    let first = fx.snapshot(v).await.unwrap();
    assert_eq!(fx.version_rows().await, 1, "the first visible build publishes the base version");
    assert_eq!(counter().await, 1);
    assert_eq!(fx.announced.0.lock().unwrap().len(), 1);

    fx.put(v, "lib-1.0-20260918.130000-2.pom", "ci", b"<project/>").await.unwrap();
    let second = fx.snapshot(v).await.unwrap();
    assert_eq!(fx.version_rows().await, 1, "the version stamp does not move for a build");
    assert_eq!(counter().await, 2);
    assert_ne!(first.etag, second.etag);
    let stamp = |e: &str| e.trim_matches('"').rsplit_once('.').unwrap().0.to_string();
    assert_eq!(stamp(&first.etag), stamp(&second.etag));
    assert_eq!(fx.announced.0.lock().unwrap().len(), 1, "announced once");
}

#[tokio::test]
async fn storage_down_during_a_deposit_is_503_and_records_nothing() {
    let fx = Fx::new().await;
    fx.storage.fail_next("writer");
    let refused = fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap_err();
    assert!(matches!(AppError::from(refused), AppError::ServiceUnavailable(_)));
    assert!(fx.unit("1.0", "").await.is_none());
    assert_eq!(fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap(), Deposited::Stored { revealed: false });
}

#[tokio::test]
async fn a_failed_version_row_leaves_the_file_served_and_unversioned() {
    let fx = Fx::new().await;
    fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Unavailable);
    let done = fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    assert_eq!(done, Deposited::Stored { revealed: true });
    assert_eq!(fx.version_rows().await, 0);
    assert_eq!(fx.store.maven().unversioned(None, 10).await.unwrap().len(), 1);
    assert_eq!(fx.announcements(), 0);
}

fn outcomes(items: &[crate::app::reconcile::Item]) -> Vec<(String, crate::app::reconcile::Reconciled)> {
    items.iter().map(|i| (i.name.clone(), i.outcome.clone())).collect()
}

#[tokio::test]
async fn a_crash_before_the_version_row_is_repaired_and_announced_once() {
    use crate::app::reconcile::{Reconciled, Reconciler};
    let fx = Fx::new().await;
    fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Unavailable);
    fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    assert_eq!((fx.version_rows().await, fx.announcements()), (0, 0), "the crash left a visible unit alone");

    let first = fx.reconciler().pass(Utc::now()).await;
    assert_eq!(outcomes(&first), vec![(format!("version {GA}:1.0"), Reconciled::Repaired)]);
    assert_eq!((fx.version_rows().await, fx.announcements()), (1, 1));
    let second = fx.reconciler().pass(Utc::now()).await;
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(fx.announcements(), 1, "one package.published");
}

#[tokio::test]
async fn a_conflict_on_one_item_does_not_stop_the_pass() {
    use crate::app::reconcile::{Reconciled, Reconciler};
    let fx = Fx::new().await;
    for v in ["1.0", "2.0"] {
        fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Unavailable);
        fx.put(v, &format!("lib-{v}.pom"), "alice", b"<project/>").await.unwrap();
    }
    fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Conflict);
    let pass = fx.reconciler().pass(Utc::now()).await;
    let outcomes = outcomes(&pass);
    assert_eq!(outcomes.len(), 2);
    assert!(matches!(outcomes[0].1, Reconciled::Failed(_)), "{outcomes:?}");
    assert_eq!(outcomes[1].1, Reconciled::Repaired, "the pass went on");
    fx.reconciler().pass(Utc::now()).await;
    assert_eq!(fx.store.maven().unversioned(None, 10).await.unwrap().len(), 0);
    assert_eq!(fx.announcements(), 2);
}

#[tokio::test]
async fn publishing_over_an_existing_base_version_does_not_abandon_the_deposit() {
    let fx = Fx::new().await;
    fx.store
        .packages()
        .publish_version(&crate::ports::packages::NewRelease {
            dependencies: &[],
            repository: fx.repo,
            package: GA,
            match_name: NameMatch::Exact,
            description: None,
            readme: None,
            version: "1.0",
            metadata_json: "{}",
            checksum_sha1: None,
            checksum_sha256: None,
            integrity: None,
            size: 1,
            tarball_path: "elsewhere",
            dist_tags: &[],
            pins: &[],
            now: Utc::now(),
        })
        .await
        .unwrap();
    let done = fx.put("1.0", "lib-1.0.pom", "alice", b"<project/>").await.unwrap();
    assert_eq!(done, Deposited::Stored { revealed: true });
    assert_eq!(fx.version_rows().await, 1);
    assert!(fx.store.maven().unversioned(None, 10).await.unwrap().is_empty());
    assert_eq!(fx.announcements(), 0);
}

#[tokio::test]
async fn a_quiet_jar_without_pom_is_promoted_once_the_window_has_passed() {
    use crate::app::reconcile::{Reconciled, Reconciler};
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap();
    let early = fx.reconciler().pass(Utc::now() + TimeDelta::minutes(5)).await;
    assert!(early.is_empty(), "{early:?}");
    let late = fx.reconciler().pass(Utc::now() + TimeDelta::minutes(11)).await;
    assert_eq!(outcomes(&late)[0].1, Reconciled::Repaired);
    assert!(fx.unit("1.0", "").await.unwrap().visible());
    assert_eq!((fx.version_rows().await, fx.announcements()), (1, 1));
}

#[tokio::test]
async fn contested_and_waiting_units_beyond_the_limit_do_not_hold_back_a_promotable_one() {
    use crate::app::reconcile::{Reconciled, Reconciler};
    let fx = Fx::new().await;
    for v in ["1.0", "1.1", "1.2"] {
        let jar = format!("lib-{v}.jar");
        fx.put(v, &jar, "alice", b"jar").await.unwrap();
        fx.put(v, &jar, "bob", b"evil").await.unwrap_err();
    }
    for v in ["1.3", "1.4", "1.5"] {
        fx.put(v, &format!("lib-{v}.jar"), "alice", b"jar").await.unwrap();
        let sum = digests(b"src").sha1;
        fx.sum(v, &format!("lib-{v}-sources.jar"), "alice", SumAlgorithm::Sha1, &sum).await.unwrap();
    }
    fx.put("2.0", "lib-2.0.jar", "alice", b"jar2").await.unwrap();

    let pass = fx.reconciler_of(2).pass(Utc::now() + TimeDelta::days(1)).await;
    assert_eq!(outcomes(&pass), vec![(format!("promote {GA}:2.0:"), Reconciled::Repaired)]);
    assert!(fx.unit("2.0", "").await.unwrap().visible());
    assert_eq!(fx.version_rows().await, 1);
}

#[tokio::test]
async fn a_value_that_keeps_failing_does_not_hold_back_the_next_one() {
    use crate::app::reconcile::{Reconciled, Reconciler};
    let fx = Fx::new().await;
    for v in ["1.0", "2.0"] {
        fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Unavailable);
        fx.put(v, &format!("lib-{v}.pom"), "alice", b"<project/>").await.unwrap();
    }
    let reconciler = fx.reconciler_of(1);
    fx.store.fail_next(crate::testing::fakes::PortId::Packages, StoreError::Unavailable);
    let first = reconciler.pass(Utc::now()).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].name, format!("version {GA}:1.0"));
    assert!(matches!(first[0].outcome, Reconciled::Failed(_)), "{first:?}");
    let second = reconciler.pass(Utc::now()).await;
    assert_eq!(outcomes(&second), vec![(format!("version {GA}:2.0"), Reconciled::Repaired)], "paged past 1.0");
}

#[tokio::test]
async fn a_contested_unit_is_never_promoted_and_an_administrator_decides() {
    use crate::app::maven::admin::{Decision, DecideUnit};
    use crate::app::reconcile::Reconciler;
    let fx = Fx::new().await;
    fx.put("1.0", "lib-1.0.jar", "alice", b"jar").await.unwrap();
    fx.put("1.0", "lib-1.0.jar", "bob", b"evil").await.unwrap_err();
    let later = fx.reconciler().pass(Utc::now() + TimeDelta::days(3)).await;
    assert!(later.is_empty(), "left to the administrator, not offered to the pass: {later:?}");
    assert!(!fx.unit("1.0", "").await.unwrap().visible());

    let decide = DecideUnit::new(fx.versions(), fx.store.audit(), crate::server::event_bus());
    let admin = crate::app::audit::Actor {
        user_id: Some(1),
        username: "root",
        admin: true,
    };
    let key = UnitKey {
        repository: fx.repo,
        ga: GA,
        version: "1.0",
        build: "",
    };
    decide.run(&key, Decision::Promote, &scopes("1.0"), &admin, Utc::now()).await.unwrap();
    assert!(fx.unit("1.0", "").await.unwrap().visible());
    assert_eq!(fx.version_rows().await, 1);
    let refused = decide.run(&key, Decision::Refuse, &scopes("1.0"), &admin, Utc::now()).await;
    assert!(matches!(refused, Err(AppError::Conflict(_))), "a published unit is not refused: {refused:?}");

    fx.put("2.0", "lib-2.0.jar", "alice", b"jar2").await.unwrap();
    let pending_key = UnitKey { version: "2.0", ..key };
    let old = fx.unit("2.0", "").await.unwrap().file("lib-2.0.jar").unwrap().physical_key.clone();
    decide.run(&pending_key, Decision::Refuse, &scopes("2.0"), &admin, Utc::now()).await.unwrap();
    assert!(fx.unit("2.0", "").await.unwrap().refused);
    assert!(fx.store.candidates().contains(&old), "released for the reclaimer, not deleted");
    let trail: Vec<String> = fx.store.audit_rows().into_iter().map(|(_, a, t)| format!("{a} {t}")).collect();
    assert_eq!(trail, [format!("maven.promote {GA}:1.0:"), format!("maven.refuse {GA}:2.0:")]);
}
