use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::TimeDelta;

use super::*;
use crate::app::reclaim::{ReclaimOrphans, ReclaimPolicy};
use crate::app::sweep_storage::SweepStorage;
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::ports::oci::{Blob, Manifest, Orphaned, UploadSession};
use crate::ports::reclaim::{Claim, PinToken, ReclaimStore};
use crate::testing::fakes::{FakeDb, PortId};
use crate::testing::storage::MemStorage;

const GRACE: Duration = Duration::from_secs(3600);

fn sha(data: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(data))
}

struct Fx {
    fakes: FakeDb,
    storage: MemStorage,
    oci: Arc<dyn OciStore>,
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
                    name: "images",
                    kind: RepoKind::Hosted,
                    format: Format::Oci,
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
            oci: db.oci(),
            fakes: db,
            storage: MemStorage::new(),
            repo: repo.id,
            prefix: layout::incarnation_prefix(&incarnation),
        }
    }

    fn storage(&self) -> Arc<dyn StorageBackend> {
        Arc::new(self.storage.clone())
    }

    fn placer(&self) -> Arc<Placer> {
        Arc::new(Placer::new(self.fakes.reclaim(), self.storage()))
    }

    fn with_oci(mut self, oci: Arc<dyn OciStore>) -> Self {
        self.oci = oci;
        self
    }

    fn append(&self) -> AppendChunk {
        AppendChunk::new(self.oci.clone(), self.storage())
    }

    fn complete(&self) -> CompleteUpload {
        CompleteUpload::new(self.oci.clone(), self.storage(), self.placer())
    }

    async fn start(&self, id: &str) {
        let prefix = layout::upload_prefix(&self.prefix, id);
        self.oci
            .start_upload(id, self.repo, "app", &prefix, Utc::now())
            .await
            .unwrap();
    }

    async fn finish(&self, id: &str, digest: &str, body: &[u8]) -> Result<(), OciWriteError> {
        self.complete()
            .run(
                Completion {
                    upload: id,
                    repository: self.repo,
                    repo_prefix: &self.prefix,
                    digest,
                    content_type: "application/octet-stream",
                    body: Bytes::copy_from_slice(body),
                },
                Utc::now(),
            )
            .await
    }

    /// A monolithic push of one blob, as a client would.
    async fn push_blob(&self, id: &str, data: &[u8]) -> String {
        self.start(id).await;
        let digest = sha(data);
        self.finish(id, &digest, data).await.unwrap();
        digest
    }

    async fn blob_key(&self, digest: &str) -> String {
        self.oci.blob(self.repo, digest).await.unwrap().unwrap().key
    }

    async fn put_manifest(&self, body: &[u8], blobs: Vec<String>, tag: &str) -> Result<(), OciWriteError> {
        let digest = sha(body);
        PutManifest::new(self.oci.clone(), self.placer())
            .run(
                PushedManifest {
                    repository: self.repo,
                    repo_prefix: &self.prefix,
                    name: "app",
                    digest: &digest,
                    content_type: "application/vnd.oci.image.manifest.v1+json",
                    blobs,
                    children: Vec::new(),
                    tag: Some(tag),
                    body: Bytes::copy_from_slice(body),
                },
                Utc::now(),
            )
            .await
    }

    fn reclaimer(&self, act_on_scan: bool) -> ReclaimOrphans {
        ReclaimOrphans::new(
            self.fakes.reclaim(),
            self.fakes.referenced(),
            self.storage(),
            ReclaimPolicy {
                grace: GRACE,
                limit: 100,
                act_on_scan,
            },
        )
    }
}

fn later() -> DateTime<Utc> {
    Utc::now() + TimeDelta::hours(5)
}

/// Chunks land as segments under the session, the completion stitches them
/// into a generation of the blob's `HostedKey`, and only then are the
/// segments deleted.
#[tokio::test]
async fn a_chunked_upload_lands_under_the_incarnation_and_drops_its_segments() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    let (a, b) = (b"first-half-".as_slice(), b"second-half".as_slice());
    let got = fx.append().run("u1", fx.repo, Some(0), Bytes::from_static(a), Utc::now()).await.unwrap();
    assert_eq!(got, a.len() as u64);
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b), Utc::now()).await.unwrap();
    let segments = fx.oci.segments("u1").await.unwrap();
    assert_eq!(segments.len(), 2);

    let whole = [a, b].concat();
    fx.finish("u1", &sha(&whole), b"").await.unwrap();

    let key = fx.blob_key(&sha(&whole)).await;
    assert!(key.starts_with(&layout::oci_blob_key(&fx.prefix, &sha(&whole)[7..])), "{key}");
    assert_eq!(fx.storage.get(&key).await.unwrap().as_ref(), whole.as_slice());
    for s in segments {
        assert!(!fx.storage.contains(&s.key), "the winner deletes its segments");
    }
    assert!(fx.oci.upload("u1").await.unwrap().is_none());
}

/// A chunk that does not start where the session stands is refused before
/// a byte is written; one that loses the compare-and-set deletes only the
/// segment it wrote.
#[tokio::test]
async fn a_chunk_out_of_range_is_refused_and_a_lost_one_deletes_its_own_segment() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    let refused = fx.append().run("u1", fx.repo, Some(4), Bytes::from_static(b"x"), Utc::now()).await;
    assert!(matches!(refused, Err(OciWriteError::OutOfRange { received: 0 })));
    assert!(fx.storage.keys().is_empty());

    fx.fakes.fail_next(PortId::Oci, StoreError::Unavailable);
    let failed = fx.append().run("u1", fx.repo, None, Bytes::from_static(b"x"), Utc::now()).await;
    assert!(failed.is_err());

    let racing = Arc::new(Racing::new(fx.fakes.oci()));
    let fx = fx.with_oci(racing.clone());
    let lost = fx.append().run("u1", fx.repo, None, Bytes::from_static(b"mine"), Utc::now()).await;
    assert!(matches!(lost, Err(OciWriteError::OutOfRange { received: 4 })), "{lost:?}");
    let deleted = fx.storage.deleted();
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].contains("/_uploads/u1/"), "{deleted:?}");
    let kept = fx.oci.segments("u1").await.unwrap();
    assert_eq!(kept.len(), 1, "the winner's segment stays");
    assert!(kept[0].key.ends_with("-theirs"));
}

/// An unknown id and an id of another repository are one 404.
#[tokio::test]
async fn a_foreign_or_legacy_upload_is_unknown() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    let foreign = fx.append().run("u1", fx.repo + 1, None, Bytes::from_static(b"x"), Utc::now()).await;
    assert!(matches!(foreign, Err(OciWriteError::NotFound)));
    fx.fakes.add_legacy_upload("old", fx.repo);
    let legacy = fx.append().run("old", fx.repo, None, Bytes::from_static(b"x"), Utc::now()).await;
    assert!(matches!(legacy, Err(OciWriteError::NotFound)));
}

#[tokio::test]
async fn a_wrong_digest_records_nothing_and_releases_the_lease() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    let wrong = fx.finish("u1", &sha(b"other"), b"data").await;
    assert!(matches!(wrong, Err(OciWriteError::DigestMismatch { .. })));
    assert!(fx.oci.blob(fx.repo, &sha(b"data")).await.unwrap().is_none());
    fx.finish("u1", &sha(b"data"), b"").await.unwrap();
}

/// A completion whose segment vanished answers the upload unknown, not a
/// storage fault.
#[tokio::test]
async fn a_vanished_segment_is_a_404_not_a_503() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"data"), Utc::now()).await.unwrap();
    let segment = fx.oci.segments("u1").await.unwrap().remove(0);
    fx.storage.vanish(&segment.key);
    let gone = fx.finish("u1", &sha(b"data"), b"").await;
    assert!(matches!(gone, Err(OciWriteError::NotFound)), "{gone:?}");
}

/// A live lease makes a second completion wait; a crashed holder's lease
/// is taken over once expired, and the lease outlives the completion bound.
#[tokio::test]
async fn completion_claim_survives_a_crash() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"data"), Utc::now()).await.unwrap();
    let ttl = fx.complete().lease_ttl();
    let BeginComplete::Lease(_) = fx.oci.begin_complete("u1", Utc::now(), ttl).await.unwrap() else {
        panic!("the first completion takes the lease");
    };
    let busy = fx.finish("u1", &sha(b"data"), b"").await;
    assert!(matches!(busy, Err(OciWriteError::Completing)));

    let expired = Utc::now() + TimeDelta::from_std(ttl).unwrap() + TimeDelta::seconds(1);
    assert!(matches!(
        fx.oci.begin_complete("u1", expired, ttl).await.unwrap(),
        BeginComplete::Lease(_)
    ));
}

#[tokio::test]
async fn lease_outlives_the_completion_bound() {
    let fx = Fx::new().await;
    assert!(fx.complete().lease_ttl() > fx.storage.upload_plan().completion_bound);
}

/// A completion overtaken after its placement records nothing and deletes
/// nothing: the new holder still reads the segments.
#[tokio::test]
async fn superseded_completion_records_nothing_and_deletes_nothing() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"data"), Utc::now()).await.unwrap();
    let hook = Arc::new(Racing::new(fx.fakes.oci()).overtaking());
    let fx = fx.with_oci(hook);
    let lost = fx.finish("u1", &sha(b"data"), b"").await;
    assert!(matches!(lost, Err(OciWriteError::Completing)), "{lost:?}");
    assert!(fx.oci.blob(fx.repo, &sha(b"data")).await.unwrap().is_none());
    let segments = fx.oci.segments("u1").await.unwrap();
    assert!(fx.storage.contains(&segments[0].key), "the segments are the new holder's source");
    assert!(fx.storage.deleted().is_empty());
    assert_eq!(fx.fakes.candidates().len(), 1, "the loser's generation is enqueued");
}

/// A pin revoked between the placement and the commit: the completion
/// re-pins a fresh generation and places it again from the segments.
#[tokio::test]
async fn superseded_pin_in_completion_replaces_from_segments() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"data"), Utc::now()).await.unwrap();
    let hook = Arc::new(Racing::new(fx.fakes.oci()).revoking(fx.fakes.reclaim(), fx.storage.clone()));
    let fx = fx.with_oci(hook.clone());
    fx.finish("u1", &sha(b"data"), b"").await.unwrap();

    let revoked = hook.revoked();
    let key = fx.blob_key(&sha(b"data")).await;
    assert_ne!(key, revoked, "a claimed generation is never pinned again");
    assert_eq!(fx.storage.get(&key).await.unwrap().as_ref(), b"data");
}

/// A blob orphaned by a manifest delete and pushed again: the new push
/// takes a fresh generation, and the reclaimer deletes only the old one.
#[tokio::test]
async fn orphan_rerecorded_by_completion_is_not_reclaimed() {
    let fx = Fx::new().await;
    let layer = fx.push_blob("u1", b"layer").await;
    let first = fx.blob_key(&layer).await;
    fx.put_manifest(b"{\"v\":1}", vec![layer.clone()], "v1").await.unwrap();
    DeleteManifest::new(fx.oci.clone())
        .run(ManifestTarget { repository: fx.repo, name: "app", digest: &sha(b"{\"v\":1}") }, Utc::now())
        .await
        .unwrap();
    assert!(fx.fakes.candidates().contains(&first));
    assert!(
        fx.storage.deleted().iter().all(|k| k.contains("/_uploads/")),
        "the request deletes nothing; only completions drop their segments"
    );

    fx.push_blob("u2", b"layer").await;
    let second = fx.blob_key(&layer).await;
    assert_ne!(first, second);

    let report = fx.reclaimer(false).run(later()).await;
    assert!(report.reclaimed >= 1, "{report:?}");
    assert!(!fx.storage.contains(&first));
    assert_eq!(fx.storage.get(&second).await.unwrap().as_ref(), b"layer");
}

/// A manifest lists only blobs the repository holds; a refused push leaves
/// its manifest generation enqueued and no row.
#[tokio::test]
async fn a_manifest_with_an_unknown_blob_is_refused_and_enqueues_its_bytes() {
    let fx = Fx::new().await;
    let refused = fx.put_manifest(b"{}", vec![sha(b"never pushed")], "v1").await;
    assert!(matches!(refused, Err(OciWriteError::BlobUnknown)), "{refused:?}");
    assert!(fx.oci.digest_for_ref(fx.repo, "app", "v1").await.unwrap().is_none());
    assert_eq!(fx.fakes.candidates().len(), 1);
}

/// The manifest's bytes land under the incarnation, its blobs are linked,
/// and a delete takes the rows and enqueues the keys, deleting nothing.
#[tokio::test]
async fn a_manifest_delete_enqueues_its_orphans_and_deletes_nothing() {
    let fx = Fx::new().await;
    let (only, shared) = (fx.push_blob("u1", b"only").await, fx.push_blob("u2", b"shared").await);
    fx.put_manifest(b"{\"a\":1}", vec![only.clone(), shared.clone()], "v1").await.unwrap();
    fx.put_manifest(b"{\"b\":1}", vec![shared.clone()], "v2").await.unwrap();
    let manifest = fx.oci.manifest(fx.repo, "app", &sha(b"{\"a\":1}")).await.unwrap().unwrap();
    assert!(manifest.key.starts_with(&fx.prefix));
    assert_eq!(fx.storage.get(&manifest.key).await.unwrap().as_ref(), b"{\"a\":1}");

    let only_key = fx.blob_key(&only).await;
    DeleteManifest::new(fx.oci.clone())
        .run(ManifestTarget { repository: fx.repo, name: "app", digest: &sha(b"{\"a\":1}") }, Utc::now())
        .await
        .unwrap();
    let mut want = vec![manifest.key.clone(), only_key.clone()];
    want.sort();
    assert_eq!(fx.fakes.candidates(), want);
    assert!(fx.storage.contains(&only_key) && fx.storage.contains(&manifest.key));
    assert_eq!(fx.oci.blob_references(fx.repo, &shared).await.unwrap(), 1);
}

#[tokio::test]
async fn a_listed_blob_is_not_deleted_and_an_unlisted_one_is_enqueued() {
    let fx = Fx::new().await;
    let layer = fx.push_blob("u1", b"layer").await;
    fx.put_manifest(b"{}", vec![layer.clone()], "v1").await.unwrap();
    let refused = DeleteBlob::new(fx.oci.clone()).run(fx.repo, &layer, Utc::now()).await;
    assert!(matches!(refused, Err(OciWriteError::Referenced)));

    let loose = fx.push_blob("u2", b"loose").await;
    let key = fx.blob_key(&loose).await;
    DeleteBlob::new(fx.oci.clone()).run(fx.repo, &loose, Utc::now()).await.unwrap();
    assert!(fx.fakes.candidates().contains(&key));
    assert!(fx.storage.contains(&key), "the request deletes nothing");
    let again = DeleteBlob::new(fx.oci.clone()).run(fx.repo, &loose, Utc::now()).await;
    assert!(matches!(again, Err(OciWriteError::NotFound)));
}

/// A slow session's segments are protected by its row, whatever their age.
#[tokio::test]
async fn scan_during_slow_oci_session_spares_its_segments() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"slow"), Utc::now()).await.unwrap();
    let segment = fx.oci.segments("u1").await.unwrap().remove(0);
    fx.storage.put("stray/object", Bytes::from_static(b"x")).await.unwrap();

    let report = fx.reclaimer(true).run(later()).await;
    assert_eq!(report.scan_orphans, 1, "{report:?}");
    fx.reclaimer(true).run(later() + TimeDelta::hours(3)).await;
    assert!(fx.storage.contains(&segment.key));
    assert!(!fx.storage.contains("stray/object"));
}

/// Stale and legacy sessions go through the sweep, bounded per pass, and
/// their prefixes through the reclaimer: nothing is reaped until it runs.
#[tokio::test]
async fn legacy_uploads_are_reclaimed_by_the_sweep_not_at_boot() {
    let fx = Fx::new().await;
    fx.fakes.add_legacy_upload("old", fx.repo);
    fx.storage.put("oci/_uploads/old/00000000000000000000", Bytes::from_static(b"x")).await.unwrap();
    fx.start("live").await;
    let sweep = SweepStorage::new(fx.storage())
        .reclaiming(fx.reclaimer(false))
        .reaping_uploads(fx.oci.clone());
    assert!(fx.fakes.candidates().is_empty(), "building the sweep reaps nothing");

    let report = sweep.run(Utc::now()).await;
    assert_eq!(report.uploads, 1, "the live session stays");
    assert_eq!(fx.fakes.candidates(), vec!["oci/_uploads/old".to_string()]);
    assert!(fx.oci.upload("live").await.unwrap().is_some());

    fx.reclaimer(false).run(later()).await;
    assert!(!fx.storage.contains("oci/_uploads/old/00000000000000000000"));
}

/// A completion whose row insert failed is retried whole by the client.
#[tokio::test]
async fn completion_is_idempotent_after_a_lost_row_insert() {
    let fx = Fx::new().await;
    fx.start("u1").await;
    fx.append().run("u1", fx.repo, None, Bytes::from_static(b"data"), Utc::now()).await.unwrap();
    let hook = Arc::new(Racing::new(fx.fakes.oci()).failing_finish());
    let fx = fx.with_oci(hook);
    assert!(fx.finish("u1", &sha(b"data"), b"").await.is_err());
    fx.finish("u1", &sha(b"data"), b"").await.unwrap();
    let key = fx.blob_key(&sha(b"data")).await;
    assert_eq!(fx.storage.get(&key).await.unwrap().as_ref(), b"data");
}

/// An `OciStore` that lets a test step in once: a chunk that wins the
/// offset just before the caller's claim, a completion overtaken or whose
/// pin is claimed just before its commit, a commit that fails.
struct Racing {
    inner: Arc<dyn OciStore>,
    once: AtomicBool,
    mode: Mode,
    revoked: std::sync::Mutex<String>,
}

enum Mode {
    ChunkRace,
    Overtake,
    Revoke(Arc<dyn ReclaimStore>, MemStorage),
    FailFinish,
}

impl Racing {
    fn new(inner: Arc<dyn OciStore>) -> Self {
        Self {
            inner,
            once: AtomicBool::new(false),
            mode: Mode::ChunkRace,
            revoked: std::sync::Mutex::new(String::new()),
        }
    }

    fn overtaking(mut self) -> Self {
        self.mode = Mode::Overtake;
        self
    }

    fn revoking(mut self, reclaim: Arc<dyn ReclaimStore>, storage: MemStorage) -> Self {
        self.mode = Mode::Revoke(reclaim, storage);
        self
    }

    fn failing_finish(mut self) -> Self {
        self.mode = Mode::FailFinish;
        self
    }

    fn revoked(&self) -> String {
        self.revoked.lock().unwrap().clone()
    }

    fn first(&self) -> bool {
        !self.once.swap(true, Ordering::SeqCst)
    }
}

#[async_trait]
impl OciStore for Racing {
    async fn blob(&self, repository: i64, digest: &str) -> Result<Option<Blob>, StoreError> {
        self.inner.blob(repository, digest).await
    }

    async fn manifest(&self, repository: i64, name: &str, digest: &str) -> Result<Option<Manifest>, StoreError> {
        self.inner.manifest(repository, name, digest).await
    }

    async fn digest_for_ref(&self, repository: i64, name: &str, reference: &str) -> Result<Option<String>, StoreError> {
        self.inner.digest_for_ref(repository, name, reference).await
    }

    async fn tags(&self, repository: i64, name: &str) -> Result<Vec<String>, StoreError> {
        self.inner.tags(repository, name).await
    }

    async fn put_manifest(&self, manifest: NewManifest<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.put_manifest(manifest, now).await
    }

    async fn delete_manifest(&self, repository: i64, name: &str, digest: &str, now: DateTime<Utc>) -> Result<Option<Orphaned>, StoreError> {
        self.inner.delete_manifest(repository, name, digest, now).await
    }

    async fn blob_references(&self, repository: i64, digest: &str) -> Result<i64, StoreError> {
        self.inner.blob_references(repository, digest).await
    }

    async fn delete_blob(&self, repository: i64, digest: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
        self.inner.delete_blob(repository, digest, now).await
    }

    async fn start_upload(&self, id: &str, repository: i64, name: &str, prefix: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.inner.start_upload(id, repository, name, prefix, now).await
    }

    async fn upload(&self, id: &str) -> Result<Option<UploadSession>, StoreError> {
        self.inner.upload(id).await
    }

    async fn claim_segment(&self, id: &str, segment: &Segment, max: u32, now: DateTime<Utc>) -> Result<SegmentClaim, StoreError> {
        if matches!(self.mode, Mode::ChunkRace) && self.first() {
            let theirs = Segment {
                start: segment.start,
                len: 4,
                key: format!("{}-theirs", segment.key),
            };
            assert_eq!(self.inner.claim_segment(id, &theirs, max, now).await?, SegmentClaim::Won);
        }
        self.inner.claim_segment(id, segment, max, now).await
    }

    async fn segments(&self, id: &str) -> Result<Vec<Segment>, StoreError> {
        self.inner.segments(id).await
    }

    async fn begin_complete(&self, id: &str, now: DateTime<Utc>, ttl: Duration) -> Result<BeginComplete, StoreError> {
        self.inner.begin_complete(id, now, ttl).await
    }

    async fn release_complete(&self, id: &str, lease: &LeaseToken) -> Result<(), StoreError> {
        self.inner.release_complete(id, lease).await
    }

    async fn finish_upload(
        &self,
        id: &str,
        lease: &LeaseToken,
        pin: &PinToken,
        blob: NewBlob<'_>,
        now: DateTime<Utc>,
    ) -> Result<Finished, StoreError> {
        if self.first() {
            match &self.mode {
                Mode::ChunkRace => {}
                Mode::Overtake => {
                    let expired = now + TimeDelta::days(1);
                    let taken = self.inner.begin_complete(id, expired, Duration::from_secs(60)).await?;
                    assert!(matches!(taken, BeginComplete::Lease(_)));
                }
                Mode::Revoke(reclaim, storage) => {
                    let key = pin.physical_key.clone();
                    reclaim.enqueue(std::slice::from_ref(&key), now - TimeDelta::hours(2)).await?;
                    let late = now + TimeDelta::days(1);
                    let claim = reclaim.claim(&key, GRACE, late, late + TimeDelta::minutes(5)).await?;
                    assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
                    storage.vanish(&key);
                    *self.revoked.lock().unwrap() = key;
                }
                Mode::FailFinish => return Err(StoreError::Unavailable),
            }
        }
        self.inner.finish_upload(id, lease, pin, blob, now).await
    }

    async fn reap_uploads(&self, idle: Duration, now: DateTime<Utc>, limit: u32) -> Result<u64, StoreError> {
        self.inner.reap_uploads(idle, now, limit).await
    }
}
