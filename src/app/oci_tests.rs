use super::*;
use crate::testing::fakes::{FakeDb, PortId};

const MANIFEST: &str = "sha256:aa";
const LAYER: &str = "sha256:bb";
const SHARED: &str = "sha256:cc";

struct Fx {
    fakes: FakeDb,
    root: tempfile::TempDir,
}

impl Fx {
    fn new() -> Self {
        Self {
            fakes: FakeDb::new(),
            root: tempfile::TempDir::new().unwrap(),
        }
    }

    fn storage(&self) -> Arc<dyn StorageBackend> {
        crate::server::filesystem(self.root.path().to_str().unwrap())
    }

    fn exists(&self, key: &str) -> bool {
        self.root.path().join(key).exists()
    }
}

fn pushed<'a>(digest: &'a str, blobs: Vec<String>, tag: Option<&'a str>) -> PushedManifest<'a> {
    PushedManifest {
        repository: 1,
        name: "app",
        digest,
        content_type: "application/vnd.oci.image.manifest.v1+json",
        blobs,
        tag,
        path: "oci/r/manifests/app/sha256/aa",
        body: Bytes::from_static(b"{}"),
    }
}

fn blob_key(digest: &str) -> String {
    format!("oci/r/_blobs/{digest}")
}

/// The bytes land before the rows, and the rows are the manifest, the layers
/// it lists and the tag that names it.
#[tokio::test]
async fn a_push_lands_the_object_then_the_manifest_its_layers_and_its_tag() {
    let fx = Fx::new();
    PutManifest::new(fx.fakes.oci(), fx.storage())
        .run(pushed(MANIFEST, vec![LAYER.to_string()], Some("v1")))
        .await
        .unwrap();

    assert!(fx.exists("oci/r/manifests/app/sha256/aa"));
    let oci = fx.fakes.oci();
    assert!(oci.manifest(1, "app", MANIFEST).await.unwrap().is_some());
    assert_eq!(
        oci.digest_for_ref(1, "app", "v1").await.unwrap().as_deref(),
        Some(MANIFEST)
    );
    assert_eq!(oci.blob_references(1, LAYER).await.unwrap(), 1);
}

/// A refused write leaves the object behind — a leak, not a corrupt registry
/// — and no row claiming it.
#[tokio::test]
async fn a_refused_push_writes_no_rows() {
    let fx = Fx::new();
    fx.fakes.fail_next(PortId::Oci, StoreError::Unavailable);

    let refused = PutManifest::new(fx.fakes.oci(), fx.storage())
        .run(pushed(MANIFEST, vec![LAYER.to_string()], Some("v1")))
        .await
        .unwrap_err();

    assert!(matches!(
        refused,
        OciWriteError::Store(StoreError::Unavailable)
    ));
    assert!(fx
        .fakes
        .oci()
        .manifest(1, "app", MANIFEST)
        .await
        .unwrap()
        .is_none());
}

/// The rows first, then the objects that deletion orphaned — and only those:
/// a layer another manifest still lists keeps its row and its file.
#[tokio::test]
async fn a_delete_removes_the_orphans_and_spares_a_shared_layer() {
    let fx = Fx::new();
    let put = PutManifest::new(fx.fakes.oci(), fx.storage());
    put.run(pushed(
        MANIFEST,
        vec![LAYER.to_string(), SHARED.to_string()],
        Some("v1"),
    ))
    .await
    .unwrap();
    put.run(pushed("sha256:dd", vec![SHARED.to_string()], Some("v2")))
        .await
        .unwrap();
    let storage = fx.storage();
    for digest in [LAYER, SHARED] {
        storage
            .put(&blob_key(digest), Bytes::from_static(b"layer"))
            .await
            .unwrap();
    }

    DeleteManifest::new(fx.fakes.oci(), fx.storage())
        .run(
            ManifestTarget {
                repository: 1,
                name: "app",
                digest: MANIFEST,
                path: "oci/r/manifests/app/sha256/aa",
            },
            blob_key,
        )
        .await
        .unwrap();

    let oci = fx.fakes.oci();
    assert!(oci.manifest(1, "app", MANIFEST).await.unwrap().is_none());
    assert!(oci.digest_for_ref(1, "app", "v1").await.unwrap().is_none());
    assert!(
        !fx.exists(&blob_key(LAYER)),
        "the orphaned layer's file goes"
    );
    assert!(
        fx.exists(&blob_key(SHARED)),
        "the shared layer's file stays"
    );
    assert_eq!(oci.blob_references(1, SHARED).await.unwrap(), 1);
}

/// Nothing to delete is `NotFound`, and the protocol adapter — not this
/// layer — spells the 404 that names the image.
#[tokio::test]
async fn deleting_an_unknown_manifest_is_not_found() {
    let fx = Fx::new();
    let refused = DeleteManifest::new(fx.fakes.oci(), fx.storage())
        .run(
            ManifestTarget {
                repository: 1,
                name: "app",
                digest: MANIFEST,
                path: "oci/r/manifests/app/sha256/aa",
            },
            blob_key,
        )
        .await
        .unwrap_err();

    assert!(matches!(refused, OciWriteError::NotFound));
}

fn assembled<'a>(upload: &'a str, digest: &'a str) -> AssembledBlob<'a> {
    AssembledBlob {
        upload,
        repository: 1,
        digest,
        content_type: "application/octet-stream",
        path: "oci/r/_blobs/sha256:bb",
        segments: vec!["oci/_uploads/u1/00000000000000000000".to_string()],
        bytes: Bytes::from_static(b"layer"),
    }
}

/// The blob row and the ledger entry go together, and the scratch the chunks
/// were assembled in does not outlive them.
#[tokio::test]
async fn a_completed_upload_records_the_blob_and_closes_the_ledger() {
    let fx = Fx::new();
    fx.fakes.oci().start_upload("u1", 1, "app").await.unwrap();
    fx.storage()
        .put("oci/_uploads/u1/00000000000000000000", Bytes::from_static(b"layer"))
        .await
        .unwrap();

    CompleteUpload::new(fx.fakes.oci(), fx.storage())
        .run(assembled("u1", LAYER))
        .await
        .unwrap();

    let oci = fx.fakes.oci();
    let blob = oci.blob(1, LAYER).await.unwrap().expect("the blob row");
    assert_eq!(blob.size, 5);
    assert!(oci.upload_owner("u1").await.unwrap().is_none());
    assert!(!fx.exists("oci/_uploads/u1/00000000000000000000"));
    assert!(fx.exists("oci/r/_blobs/sha256:bb"));
}

/// A layer a live image still lists is refused with the count the adapter
/// puts in its 409, and its object stays.
#[tokio::test]
async fn a_referenced_blob_is_refused_and_keeps_its_object() {
    let fx = Fx::new();
    PutManifest::new(fx.fakes.oci(), fx.storage())
        .run(pushed(MANIFEST, vec![LAYER.to_string()], None))
        .await
        .unwrap();
    fx.storage()
        .put(&blob_key(LAYER), Bytes::from_static(b"layer"))
        .await
        .unwrap();

    let refused = DeleteBlob::new(fx.fakes.oci(), fx.storage())
        .run(1, LAYER, &blob_key(LAYER))
        .await
        .unwrap_err();

    assert!(matches!(refused, OciWriteError::Referenced(1)));
    assert!(fx.exists(&blob_key(LAYER)));
}

/// An unreferenced blob loses its row and then its object; an unknown one is
/// `NotFound`.
#[tokio::test]
async fn an_unreferenced_blob_goes_and_an_unknown_one_is_not_found() {
    let fx = Fx::new();
    fx.fakes.add_blob(1, LAYER, 5, None);
    fx.storage()
        .put(&blob_key(LAYER), Bytes::from_static(b"layer"))
        .await
        .unwrap();
    let delete = DeleteBlob::new(fx.fakes.oci(), fx.storage());

    delete.run(1, LAYER, &blob_key(LAYER)).await.unwrap();
    assert!(fx.fakes.oci().blob(1, LAYER).await.unwrap().is_none());
    assert!(!fx.exists(&blob_key(LAYER)));

    let refused = delete.run(1, LAYER, &blob_key(LAYER)).await.unwrap_err();
    assert!(matches!(refused, OciWriteError::NotFound));
}
