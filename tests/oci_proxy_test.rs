mod common;

use std::sync::atomic::Ordering;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::upstream_tap::{self, Tap};
use common::{
    expire_entries, hosted, proxy, push_blob, sha256_digest, spawn_server, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const UPSTREAM_REPO: &str = "oci-hosted";
const IMAGE: &str = "team/app";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// A second opencargo holding `team/app:1.0`, fronted by a tap.
struct Upstream {
    server: TestServer,
    tap: Tap,
    manifest: Vec<u8>,
    layer: Vec<u8>,
}

impl Upstream {
    fn url(&self) -> String {
        format!("{}/{UPSTREAM_REPO}", self.tap.base_url)
    }

    fn digest(&self) -> String {
        sha256_digest(&self.manifest)
    }

    fn layer_digest(&self) -> String {
        sha256_digest(&self.layer)
    }

    fn manifest_path(&self, reference: &str) -> String {
        format!("/v2/{UPSTREAM_REPO}/{IMAGE}/manifests/{reference}")
    }

    fn blob_path(&self, digest: &str) -> String {
        format!("/v2/{UPSTREAM_REPO}/{IMAGE}/blobs/{digest}")
    }

    fn hits(&self) -> usize {
        self.tap.hits.lock().unwrap().len()
    }
}

fn manifest_for(config: &[u8], layer: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": sha256_digest(config),
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": sha256_digest(layer),
            "size": layer.len()
        }]
    }))
    .unwrap()
}

async fn put_manifest(base_url: &str, image: &str, reference: &str, manifest: &[u8]) {
    let resp = reqwest::Client::new()
        .put(format!("{base_url}/v2/{image}/manifests/{reference}"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", MANIFEST_TYPE)
        .body(manifest.to_vec())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED, "manifest push");
}

/// Push config + layer + `tag` under `image`; returns (manifest, layer).
async fn seed_image(base_url: &str, image: &str, tag: &str, layer: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let client = reqwest::Client::new();
    let config = format!("{{\"image\":\"{image}\",\"tag\":\"{tag}\"}}").into_bytes();
    push_blob(&client, base_url, image, &config).await;
    push_blob(&client, base_url, image, layer).await;
    let manifest = manifest_for(&config, layer);
    put_manifest(base_url, image, tag, &manifest).await;
    (manifest, layer.to_vec())
}

async fn seed_upstream() -> Upstream {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted(UPSTREAM_REPO, RepositoryFormat::Oci, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let image = format!("{UPSTREAM_REPO}/{IMAGE}");
    let (manifest, layer) = seed_image(&server.base_url, &image, "1.0", b"layer-1.0").await;
    let tap = upstream_tap::start(&server.base_url).await;
    Upstream {
        server,
        tap,
        manifest,
        layer,
    }
}

async fn spawn_proxy(up: &Upstream) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![proxy("oci-proxy", RepositoryFormat::Oci, &up.url())],
        ..Default::default()
    })
    .await
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name} header"))
        .to_str()
        .expect("non-ascii header")
        .to_string()
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::get(url).await.expect("request failed")
}

async fn head(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .head(url)
        .send()
        .await
        .expect("request failed")
}

/// `(kind, cache_key, status)` of every cache row the server holds.
async fn cache_rows(server: &TestServer) -> Vec<(String, String, i64)> {
    let pool = open_db(server).await;
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT kind, cache_key, status FROM proxy_cache_entries ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read cache rows");
    pool.close().await;
    rows
}

async fn oci_table_rows(server: &TestServer) -> i64 {
    let pool = open_db(server).await;
    let n: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM oci_blobs) + (SELECT COUNT(*) FROM oci_manifests) \
         + (SELECT COUNT(*) FROM oci_tags)",
    )
    .fetch_one(&pool)
    .await
    .expect("failed to count oci rows");
    pool.close().await;
    n
}

async fn open_db(server: &TestServer) -> sqlx::SqlitePool {
    let db_path = server.tmp.path().join("opencargo.db");
    sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database")
}

#[tokio::test]
async fn proxy_pull_by_tag_then_by_digest_from_second_instance() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);

    let resp = get(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());
    assert_eq!(header(&resp, "content-type"), MANIFEST_TYPE);
    assert_eq!(resp.bytes().await.unwrap(), up.manifest.as_slice());

    let resp = get(&format!("{base}/manifests/{}", up.digest())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());
    assert_eq!(resp.bytes().await.unwrap(), up.manifest.as_slice());
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 1);
    assert_eq!(up.tap.count(&up.manifest_path(&up.digest())), 0, "digest pull is local");

    let rows = cache_rows(&a).await;
    let kinds: Vec<&str> = rows.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(kinds, ["oci-manifest", "oci-tag"], "{rows:?}");
    assert_eq!(rows[1].1, format!("{IMAGE}/1.0"));

    let blob_url = format!("{base}/blobs/{}", up.layer_digest());
    for _ in 0..2 {
        let resp = get(&blob_url).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "docker-content-digest"), up.layer_digest());
        assert_eq!(header(&resp, "content-length"), up.layer.len().to_string());
        assert_eq!(resp.bytes().await.unwrap(), up.layer.as_slice());
    }
    assert_eq!(up.tap.count(&up.blob_path(&up.layer_digest())), 1);
    assert_eq!(oci_table_rows(&a).await, 0, "proxied artifacts never enter oci_* tables");
}

#[tokio::test]
async fn tag_revalidation_updates_after_upstream_retag() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/v2/oci-proxy/{IMAGE}/manifests/1.0", a.base_url);

    let resp = get(&url).await;
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());

    let image = format!("{UPSTREAM_REPO}/{IMAGE}");
    let (retagged, _) = seed_image(&up.server.base_url, &image, "1.0", b"layer-1.0-rebuilt").await;
    let new_digest = sha256_digest(&retagged);
    assert_ne!(new_digest, up.digest());

    let resp = get(&url).await;
    assert_eq!(header(&resp, "docker-content-digest"), up.digest(), "fresh tag row");
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 1);

    expire_entries(&a).await;
    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), new_digest);
    assert_eq!(resp.bytes().await.unwrap(), retagged.as_slice());
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 2);
}

#[tokio::test]
async fn head_blob_hit_is_served_from_cache_without_upstream() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);

    let manifest: Value = get(&format!("{base}/manifests/1.0")).await.json().await.unwrap();
    let mut layers: Vec<(String, u64)> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["digest"].as_str().unwrap().to_string(), l["size"].as_u64().unwrap()))
        .collect();
    layers.push((
        manifest["config"]["digest"].as_str().unwrap().to_string(),
        manifest["config"]["size"].as_u64().unwrap(),
    ));
    for (digest, _) in &layers {
        assert_eq!(get(&format!("{base}/blobs/{digest}")).await.status(), StatusCode::OK);
    }

    let before = up.hits();
    up.tap.fail.store(true, Ordering::SeqCst);
    for (digest, size) in &layers {
        let resp = head(&format!("{base}/blobs/{digest}")).await;
        assert_eq!(resp.status(), StatusCode::OK, "{digest}");
        assert_eq!(header(&resp, "content-length"), size.to_string());
        assert_eq!(header(&resp, "docker-content-digest"), *digest);
    }
    let resp = head(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());
    assert_eq!(header(&resp, "content-length"), up.manifest.len().to_string());
    assert_eq!(up.hits(), before, "a warm cache answers HEAD with zero upstream traffic");
}

#[tokio::test]
async fn unknown_manifest_negative_cached() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/v2/oci-proxy/{IMAGE}/manifests/nope", a.base_url);

    for _ in 0..2 {
        assert_eq!(get(&url).await.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(up.tap.count(&up.manifest_path("nope")), 1);
    let rows = cache_rows(&a).await;
    assert_eq!(rows, vec![("oci-tag".to_string(), format!("{IMAGE}/nope"), 404)]);
}

#[tokio::test]
async fn upstream_503_serves_stale_tag() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let url = format!("{}/v2/oci-proxy/{IMAGE}/manifests/1.0", a.base_url);

    assert_eq!(get(&url).await.status(), StatusCode::OK);
    expire_entries(&a).await;
    up.tap.fail.store(true, Ordering::SeqCst);

    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(header(&resp, "warning").starts_with("110"), "stale warning");
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());
    assert_eq!(resp.bytes().await.unwrap(), up.manifest.as_slice());
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 2);
}

#[tokio::test]
async fn oci_proxy_refuses_client_delete() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);
    assert_eq!(get(&format!("{base}/manifests/1.0")).await.status(), StatusCode::OK);

    let client = reqwest::Client::new();
    for url in [
        format!("{base}/manifests/1.0"),
        format!("{base}/blobs/{}", up.layer_digest()),
    ] {
        let resp = client
            .delete(&url)
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{url}");
    }
    assert_eq!(get(&format!("{base}/manifests/1.0")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn oci_proxy_purge_removes_rows_and_files_and_leaves_oci_tables_untouched() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);

    assert_eq!(get(&format!("{base}/manifests/1.0")).await.status(), StatusCode::OK);
    assert_eq!(
        get(&format!("{base}/blobs/{}", up.layer_digest())).await.status(),
        StatusCode::OK
    );
    let cache_dir = a.tmp.path().join("storage/_proxy_cache/oci-proxy");
    assert_eq!(cache_rows(&a).await.len(), 3);
    assert!(cache_dir.is_dir());
    assert_eq!(oci_table_rows(&a).await, 0);

    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/repositories/oci-proxy/purge-cache", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    assert!(cache_rows(&a).await.is_empty());
    assert!(!cache_dir.exists(), "purge removes the member's cache directory");
    assert_eq!(oci_table_rows(&a).await, 0);
    assert_eq!(oci_table_rows(&up.server).await, 4, "the upstream's rows are untouched");

    let resp = get(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 2, "purge forces a refetch");
}
