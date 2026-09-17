mod common;

use std::sync::atomic::Ordering;

use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::Digest;

use common::fake_upstream::oci::{self as fake_oci, Blob, FakeRegistry, Options};
use common::upstream_tap::{self, Tap};
use common::{
    expire_entries, group, hosted, proxy, proxy_with, push_blob, respawn, sha256_digest,
    spawn_server, ProxyOpts, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{ProxyConfig, RepositoryFormat, Visibility};
use opencargo::proxy::{UpstreamAuth, UpstreamStrategy};
use opencargo::registry::oci::upstream::{upstream_name, OciArtifact, OciUpstream};

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
        repositories: vec![hosted(
            UPSTREAM_REPO,
            RepositoryFormat::Oci,
            Visibility::Public,
        )],
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
    assert_eq!(
        up.tap.count(&up.manifest_path(&up.digest())),
        0,
        "digest pull is local"
    );

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
    expire_entries(&a).await;
    let resp = get(&blob_url).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), up.layer.as_slice());
    let resp = get(&format!("{base}/manifests/{}", up.digest())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(up.tap.count(&up.blob_path(&up.layer_digest())), 1, "blobs are immutable");
    assert_eq!(
        up.tap.count(&up.manifest_path(&up.digest())),
        0,
        "so is a manifest by digest"
    );
    assert_eq!(
        oci_table_rows(&a).await,
        0,
        "proxied artifacts never enter oci_* tables"
    );
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
    assert_eq!(
        header(&resp, "docker-content-digest"),
        up.digest(),
        "fresh tag row"
    );
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

    let manifest: Value = get(&format!("{base}/manifests/1.0"))
        .await
        .json()
        .await
        .unwrap();
    let mut layers: Vec<(String, u64)> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["digest"].as_str().unwrap().to_string(),
                l["size"].as_u64().unwrap(),
            )
        })
        .collect();
    layers.push((
        manifest["config"]["digest"].as_str().unwrap().to_string(),
        manifest["config"]["size"].as_u64().unwrap(),
    ));
    for (digest, _) in &layers {
        assert_eq!(
            get(&format!("{base}/blobs/{digest}")).await.status(),
            StatusCode::OK
        );
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
    assert_eq!(
        header(&resp, "content-length"),
        up.manifest.len().to_string()
    );
    assert_eq!(
        up.hits(),
        before,
        "a warm cache answers HEAD with zero upstream traffic"
    );
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
    assert_eq!(
        rows,
        vec![("oci-tag".to_string(), format!("{IMAGE}/nope"), 404)]
    );
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
    assert_eq!(
        get(&format!("{base}/manifests/1.0")).await.status(),
        StatusCode::OK
    );

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
    assert_eq!(
        get(&format!("{base}/manifests/1.0")).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn oci_proxy_purge_removes_rows_and_files_and_leaves_oci_tables_untouched() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);

    assert_eq!(
        get(&format!("{base}/manifests/1.0")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&format!("{base}/blobs/{}", up.layer_digest()))
            .await
            .status(),
        StatusCode::OK
    );
    let cache_dir = a.tmp.path().join("storage/_proxy_cache/oci-proxy");
    assert_eq!(cache_rows(&a).await.len(), 3);
    assert!(cache_dir.is_dir());
    assert_eq!(oci_table_rows(&a).await, 0);

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/repositories/oci-proxy/purge-cache",
            a.base_url
        ))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    assert!(cache_rows(&a).await.is_empty());
    assert!(
        !cache_dir.exists(),
        "purge removes the member's cache directory"
    );
    assert_eq!(oci_table_rows(&a).await, 0);
    assert_eq!(
        oci_table_rows(&up.server).await,
        4,
        "the upstream's rows are untouched"
    );

    let resp = get(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        up.tap.count(&up.manifest_path("1.0")),
        2,
        "purge forces a refetch"
    );
}

/// A hosted member holding `team/app:local` beside a proxy member fronting
/// the second instance, under one group.
async fn spawn_group(up: &Upstream) -> (TestServer, Vec<u8>, Vec<u8>) {
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("oci-local", RepositoryFormat::Oci, Visibility::Public),
            proxy("oci-proxy", RepositoryFormat::Oci, &up.url()),
            group(
                "oci-group",
                RepositoryFormat::Oci,
                &["oci-local", "oci-proxy"],
            ),
        ],
        ..Default::default()
    })
    .await;
    let (manifest, layer) = seed_image(
        &a.base_url,
        &format!("oci-local/{IMAGE}"),
        "local",
        b"layer-local",
    )
    .await;
    (a, manifest, layer)
}

async fn tags_via(base_url: &str, image: &str, query: &str) -> Value {
    let resp = get(&format!("{base_url}/v2/{image}/tags/list{query}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn group_manifest_blob_tags_through_group() {
    let up = seed_upstream().await;
    let (a, local_manifest, local_layer) = spawn_group(&up).await;
    let base = format!("{}/v2/oci-group/{IMAGE}", a.base_url);

    for (reference, manifest) in [("1.0", &up.manifest), ("local", &local_manifest)] {
        let resp = get(&format!("{base}/manifests/{reference}")).await;
        assert_eq!(resp.status(), StatusCode::OK, "{reference}");
        assert_eq!(
            header(&resp, "docker-content-digest"),
            sha256_digest(manifest)
        );
        assert_eq!(resp.bytes().await.unwrap(), manifest.as_slice());
    }
    for layer in [&up.layer, &local_layer] {
        let resp = get(&format!("{base}/blobs/{}", sha256_digest(layer))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.bytes().await.unwrap(), layer.as_slice());
    }
    assert_eq!(up.tap.count(&up.manifest_path("1.0")), 1);
    assert_eq!(
        up.tap.count(&up.manifest_path("local")),
        0,
        "the hosted member wins"
    );

    let image = format!("oci-group/{IMAGE}");
    assert_eq!(
        tags_via(&a.base_url, &image, "").await,
        json!({ "name": image, "tags": ["1.0", "local"] })
    );
    assert_eq!(
        tags_via(&a.base_url, &image, "?n=1").await["tags"],
        json!(["1.0"])
    );
    assert_eq!(
        tags_via(&a.base_url, &image, "?last=1.0").await["tags"],
        json!(["local"])
    );
    assert_eq!(
        tags_via(&a.base_url, "oci-group/team/unknown", "").await,
        json!({ "name": "oci-group/team/unknown", "tags": [] })
    );
    assert_eq!(
        get(&format!("{base}/manifests/nope")).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn oci_group_location_and_digest_headers_use_group() {
    let up = seed_upstream().await;
    let (a, ..) = spawn_group(&up).await;
    let base = format!("{}/v2/oci-group/{IMAGE}", a.base_url);

    let resp = head(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), up.digest());
    assert_eq!(header(&resp, "content-type"), MANIFEST_TYPE);
    assert_eq!(
        header(&resp, "content-length"),
        up.manifest.len().to_string()
    );

    let resp = head(&format!("{base}/blobs/{}", up.layer_digest())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "docker-content-digest"), up.layer_digest());

    let tags = tags_via(&a.base_url, &format!("oci-group/{IMAGE}"), "").await;
    assert_eq!(
        tags["name"],
        format!("oci-group/{IMAGE}"),
        "never a member name"
    );
    let body = tags.to_string();
    assert!(
        !body.contains("oci-proxy") && !body.contains("oci-local"),
        "{body}"
    );
}

#[tokio::test]
async fn push_to_group_is_400() {
    let up = seed_upstream().await;
    let (a, local_manifest, _) = spawn_group(&up).await;
    let client = reqwest::Client::new();
    let base = format!("{}/v2/oci-group/{IMAGE}", a.base_url);

    let resp = client
        .post(format!("{base}/blobs/uploads/"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = client
        .put(format!("{base}/manifests/2.0"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", MANIFEST_TYPE)
        .body(local_manifest)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = client
        .delete(format!("{base}/manifests/local"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        get(&format!("{base}/manifests/local")).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn group_hides_private_member() {
    let repos = || {
        vec![
            hosted("oci-secret", RepositoryFormat::Oci, Visibility::Private),
            hosted("oci-empty", RepositoryFormat::Oci, Visibility::Public),
            group(
                "oci-group",
                RepositoryFormat::Oci,
                &["oci-secret", "oci-empty"],
            ),
        ]
    };
    let a = spawn_server(SpawnOpts {
        repositories: repos(),
        ..Default::default()
    })
    .await;
    seed_image(
        &a.base_url,
        &format!("oci-secret/{IMAGE}"),
        "1.0",
        b"secret-layer",
    )
    .await;
    let via_group = format!("{}/v2/oci-group/{IMAGE}/manifests/1.0", a.base_url);

    assert_eq!(
        get(&via_group).await.status(),
        StatusCode::NOT_FOUND,
        "hidden, not 403"
    );
    let direct = format!("{}/v2/oci-secret/{IMAGE}/manifests/1.0", a.base_url);
    assert_eq!(get(&direct).await.status(), StatusCode::UNAUTHORIZED);
    let resp = reqwest::Client::new()
        .get(&via_group)
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "a reader sees the member");

    let closed = spawn_server(SpawnOpts {
        anonymous_read: false,
        repositories: repos(),
        ..Default::default()
    })
    .await;
    let resp = get(&format!(
        "{}/v2/oci-group/{IMAGE}/manifests/1.0",
        closed.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        header(&resp, "www-authenticate").starts_with("Basic"),
        "docker login shape"
    );
}

#[tokio::test]
async fn tags_list_ttl() {
    let up = seed_upstream().await;
    let a = spawn_proxy(&up).await;
    let image = format!("oci-proxy/{IMAGE}");
    let path = format!("/v2/{UPSTREAM_REPO}/{IMAGE}/tags/list?n=10000");

    for _ in 0..2 {
        assert_eq!(
            tags_via(&a.base_url, &image, "").await["tags"],
            json!(["1.0"])
        );
    }
    assert_eq!(up.tap.count(&path), 1);

    seed_image(
        &up.server.base_url,
        &format!("{UPSTREAM_REPO}/{IMAGE}"),
        "2.0",
        b"layer-2.0",
    )
    .await;
    assert_eq!(
        tags_via(&a.base_url, &image, "").await["tags"],
        json!(["1.0"]),
        "fresh row"
    );
    expire_entries(&a).await;
    assert_eq!(
        tags_via(&a.base_url, &image, "").await["tags"],
        json!(["1.0", "2.0"])
    );
    assert_eq!(up.tap.count(&path), 2);
    assert!(cache_rows(&a)
        .await
        .iter()
        .any(|(k, key, _)| k == "oci-tags" && key == IMAGE));
}

/// A fake registry holding `team/app:1.0` (one config, one layer).
struct Fake {
    reg: FakeRegistry,
    manifest: Vec<u8>,
    layer: Vec<u8>,
}

impl Fake {
    fn layer_digest(&self) -> String {
        sha256_digest(&self.layer)
    }

    fn manifest_path(&self) -> String {
        format!("/v2/{IMAGE}/manifests/1.0")
    }

    fn blob_path(&self, digest: &str) -> String {
        format!("/v2/{IMAGE}/blobs/{digest}")
    }
}

async fn fake_with_image(opts: Options) -> Fake {
    let reg = fake_oci::start(opts).await;
    let layer = b"fake-layer".to_vec();
    let config = b"{}".to_vec();
    reg.add_blob(IMAGE, Blob::Bytes(layer.clone()));
    reg.add_blob(IMAGE, Blob::Bytes(config.clone()));
    let manifest = manifest_for(&config, &layer);
    reg.add_manifest(IMAGE, Some("1.0"), &manifest, MANIFEST_TYPE);
    Fake {
        reg,
        manifest,
        layer,
    }
}

async fn spawn_fake_proxy(
    reg: &FakeRegistry,
    opts: ProxyOpts,
    connect_timeout: &str,
) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![proxy_with(
            "oci-proxy",
            RepositoryFormat::Oci,
            &reg.base_url,
            opts,
        )],
        proxy: ProxyConfig {
            connect_timeout: connect_timeout.to_string(),
            ..Default::default()
        },
        ..Default::default()
    })
    .await
}

fn basic(user: &str, pass: &str) -> ProxyOpts {
    ProxyOpts {
        upstream_auth: Some(UpstreamAuth::Basic {
            username: user.to_string(),
            password: pass.to_string(),
        }),
        ..Default::default()
    }
}

fn cache_files(server: &TestServer) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![server.tmp.path().join("storage/_proxy_cache")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn hub_token_dance_with_fake_registry() {
    let fake = fake_with_image(Options {
        challenge: true,
        realm_basic: Some(("hub".into(), "secret".into())),
        ..Default::default()
    })
    .await;
    let a = spawn_fake_proxy(&fake.reg, basic("hub", "secret"), "10s").await;
    let base = format!("{}/v2/oci-proxy/{IMAGE}", a.base_url);

    let resp = get(&format!("{base}/manifests/1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), fake.manifest.as_slice());
    let resp = get(&format!("{base}/blobs/{}", fake.layer_digest())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), fake.layer.as_slice());

    assert_eq!(fake.reg.tokens_issued(), 1, "one token for two pulls");
    assert_eq!(
        fake.reg.count(reqwest::Method::GET, &fake.manifest_path()),
        2,
        "challenge, retry"
    );
    let blob_hits: Vec<_> = fake
        .reg
        .hits()
        .into_iter()
        .filter(|h| h.path == fake.blob_path(&fake.layer_digest()))
        .collect();
    assert_eq!(blob_hits.len(), 1, "the cached token skips the challenge");
    assert!(blob_hits[0].headers["authorization"]
        .to_str()
        .unwrap()
        .starts_with("Bearer tok-"));
}

#[tokio::test]
async fn token_cache_never_crosses_repositories() {
    let fake = fake_with_image(Options {
        challenge: true,
        realm_basic: Some(("hub".into(), "secret".into())),
        ..Default::default()
    })
    .await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            proxy_with(
                "oci-proxy",
                RepositoryFormat::Oci,
                &fake.reg.base_url,
                basic("hub", "secret"),
            ),
            proxy("oci-anon", RepositoryFormat::Oci, &fake.reg.base_url),
        ],
        ..Default::default()
    })
    .await;

    let resp = get(&format!("{}/v2/oci-proxy/{IMAGE}/manifests/1.0", a.base_url)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(fake.reg.tokens_issued(), 1);

    let resp = get(&format!("{}/v2/oci-anon/{IMAGE}/manifests/1.0", a.base_url)).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "the anonymous proxy must not replay the credentialed proxy's token"
    );
    assert_eq!(fake.reg.tokens_issued(), 1, "the realm refused it a token");
    let bearer_pulls = fake
        .reg
        .hits()
        .into_iter()
        .filter(|h| h.path == fake.manifest_path())
        .filter(|h| {
            h.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("Bearer tok-"))
        })
        .count();
    assert_eq!(bearer_pulls, 1, "one token-bearing pull, by oci-proxy");
}

/// Credentials never travel through the API: a proxy created there takes
/// `OPENCARGO_UPSTREAM_AUTH_<REPO>` from the environment at the next start.
#[tokio::test]
async fn api_created_proxy_takes_env_credentials_on_restart() {
    let fake = fake_with_image(Options {
        challenge: true,
        realm_basic: Some(("hub".into(), "secret".into())),
        ..Default::default()
    })
    .await;
    let a = spawn_server(SpawnOpts::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/repositories", a.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({
            "name": "oci-api-made", "type": "proxy", "format": "oci",
            "visibility": "public", "upstream": fake.reg.base_url,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let url_for = |a: &TestServer| format!("{}/v2/oci-api-made/{IMAGE}/manifests/1.0", a.base_url);
    assert_eq!(get(&url_for(&a)).await.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(fake.reg.tokens_issued(), 0);

    std::env::set_var("OPENCARGO_UPSTREAM_AUTH_OCI_API_MADE", "basic:hub:secret");
    let a = respawn(a, SpawnOpts::default()).await;
    assert_eq!(get(&url_for(&a)).await.status(), StatusCode::OK);
    assert_eq!(fake.reg.tokens_issued(), 1);
}

#[tokio::test]
async fn accept_header_sent_upstream() {
    let fake = fake_with_image(Options::default()).await;
    let a = spawn_fake_proxy(&fake.reg, ProxyOpts::default(), "10s").await;

    let resp = get(&format!(
        "{}/v2/oci-proxy/{IMAGE}/manifests/1.0",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let hit = fake
        .reg
        .hits()
        .into_iter()
        .find(|h| h.path == fake.manifest_path())
        .expect("manifest fetched upstream");
    let accept = hit.headers["accept"].to_str().unwrap().to_string();
    for media in [
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
    ] {
        assert!(accept.contains(media), "{accept}");
    }
}

#[tokio::test]
async fn library_prefix_only_for_docker_hub_hosts() {
    let reg = fake_oci::start(Options::default()).await;
    let manifest = manifest_for(b"{}", b"alpine-layer");
    reg.add_manifest("alpine", Some("3.20"), &manifest, MANIFEST_TYPE);
    let a = spawn_fake_proxy(&reg, ProxyOpts::default(), "10s").await;

    let resp = get(&format!(
        "{}/v2/oci-proxy/alpine/manifests/3.20",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        reg.count(reqwest::Method::GET, "/v2/alpine/manifests/3.20"),
        1
    );
    assert_eq!(
        reg.count(reqwest::Method::GET, "/v2/library/alpine/manifests/3.20"),
        0
    );

    let hub = opencargo::registry::resolve::Upstream {
        base: reqwest::Url::parse("https://docker.io").unwrap(),
        auth: None,
        token_realms: Vec::new(),
        dl_allow_private: false,
    };
    let tag = OciArtifact::Tag {
        name: upstream_name(&hub, "alpine"),
        tag: "3.20".into(),
    };
    assert_eq!(
        OciUpstream.upstream_url(&hub, &tag).unwrap().as_str(),
        "https://registry-1.docker.io/v2/library/alpine/manifests/3.20"
    );
    assert_eq!(
        OciUpstream.bearer_scope(&tag).unwrap(),
        "repository:library/alpine:pull"
    );
}

#[tokio::test]
async fn blob_larger_than_buffer_cap_streams_through() {
    const SIZE: usize = 300 * 1024 * 1024;
    let reg = fake_oci::start(Options::default()).await;
    let digest = reg.add_blob(IMAGE, Blob::Pattern { size: SIZE });
    let a = spawn_fake_proxy(&reg, ProxyOpts::default(), "10s").await;

    let mut resp = get(&format!(
        "{}/v2/oci-proxy/{IMAGE}/blobs/{digest}",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "content-length"), SIZE.to_string());
    let mut hasher = sha2::Sha256::new();
    let mut received = 0usize;
    while let Some(chunk) = resp.chunk().await.unwrap() {
        hasher.update(&chunk);
        received += chunk.len();
    }
    assert_eq!(received, SIZE, "size == Content-Length");
    assert_eq!(format!("sha256:{:x}", hasher.finalize()), digest);
    let rows = cache_rows(&a).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "oci-blob");
}

#[tokio::test]
async fn blob_digest_mismatch_is_502_nothing_stored() {
    let reg = fake_oci::start(Options::default()).await;
    let claimed = sha256_digest(b"what the registry claims");
    reg.add_blob_as(
        IMAGE,
        &claimed,
        Blob::Bytes(b"what it actually sends".to_vec()),
    );
    let a = spawn_fake_proxy(&reg, ProxyOpts::default(), "10s").await;

    let resp = get(&format!(
        "{}/v2/oci-proxy/{IMAGE}/blobs/{claimed}",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(cache_rows(&a).await.is_empty(), "no row");
    assert!(cache_files(&a).is_empty(), "no file, no part left behind");
}

#[tokio::test]
async fn slow_drip_blob_past_buffered_total_completes() {
    let reg = fake_oci::start(Options::default()).await;
    let blob = Blob::Trickle {
        chunks: 16,
        every: std::time::Duration::from_millis(500),
    };
    let expected = blob.digest();
    let digest = reg.add_blob(IMAGE, blob);
    assert_eq!(digest, expected);
    let a = spawn_fake_proxy(&reg, ProxyOpts::default(), "1s").await;

    let started = std::time::Instant::now();
    let resp = get(&format!(
        "{}/v2/oci-proxy/{IMAGE}/blobs/{digest}",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), b"drip".repeat(16).as_slice());
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(7),
        "the drip was not cut short"
    );
}

#[tokio::test]
async fn second_puller_proceeds_after_singleflight_wait() {
    let reg = fake_oci::start(Options::default()).await;
    let digest = reg.add_blob(
        IMAGE,
        Blob::Trickle {
            chunks: 8,
            every: std::time::Duration::from_secs(1),
        },
    );
    let a = spawn_fake_proxy(&reg, ProxyOpts::default(), "1s").await;
    let url = format!("{}/v2/oci-proxy/{IMAGE}/blobs/{digest}", a.base_url);

    let pull = |url: String| async move { get(&url).await.bytes().await };
    let leader = tokio::spawn(pull(url.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let waiter = tokio::spawn(pull(url));
    let (leader, waiter) = (
        leader.await.unwrap().unwrap(),
        waiter.await.unwrap().unwrap(),
    );
    assert_eq!(leader, b"drip".repeat(8).as_slice());
    assert_eq!(waiter, leader);
    assert_eq!(
        reg.count(reqwest::Method::GET, &format!("/v2/{IMAGE}/blobs/{digest}")),
        2,
        "the waiter past singleflight_wait downloads on its own"
    );
}

#[tokio::test]
async fn head_blob_miss_forwards_head_without_download() {
    let fake = fake_with_image(Options {
        challenge: true,
        ..Default::default()
    })
    .await;
    let a = spawn_fake_proxy(&fake.reg, ProxyOpts::default(), "10s").await;
    let path = fake.blob_path(&fake.layer_digest());

    let resp = head(&format!(
        "{}/v2/oci-proxy/{IMAGE}/blobs/{}",
        a.base_url,
        fake.layer_digest()
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "content-length"), fake.layer.len().to_string());
    assert_eq!(header(&resp, "docker-content-digest"), fake.layer_digest());
    assert_eq!(
        fake.reg.count(reqwest::Method::HEAD, &path),
        2,
        "challenge, then HEAD with token"
    );
    assert_eq!(
        fake.reg.count(reqwest::Method::GET, &path),
        0,
        "never downloaded"
    );
    assert!(cache_rows(&a).await.is_empty() && cache_files(&a).is_empty());
}

/// Hub answers 401 after a token for an unknown or private image: a 404 to
/// the client, never a negative row, so the image is visible as soon as
/// access is (a fixed credential, a made-public image).
#[tokio::test]
async fn unknown_repository_401_after_token_is_404_and_asked_again() {
    let fake = fake_with_image(Options {
        challenge: true,
        hub_shape: true,
        ..Default::default()
    })
    .await;
    let a = spawn_fake_proxy(&fake.reg, ProxyOpts::default(), "10s").await;
    let url = format!("{}/v2/oci-proxy/team/ghost/manifests/1.0", a.base_url);

    assert_eq!(get(&url).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(fake.reg.tokens_issued(), 1);
    assert!(cache_rows(&a).await.is_empty(), "a refusal is not remembered");
    assert_eq!(head(&url).await.status(), StatusCode::NOT_FOUND);

    let layer = b"ghost-layer".to_vec();
    let config = b"{}".to_vec();
    fake.reg.add_blob("team/ghost", Blob::Bytes(layer.clone()));
    fake.reg.add_blob("team/ghost", Blob::Bytes(config.clone()));
    let manifest = manifest_for(&config, &layer);
    fake.reg
        .add_manifest("team/ghost", Some("1.0"), &manifest, MANIFEST_TYPE);

    let resp = get(&url).await;
    assert_eq!(resp.status(), StatusCode::OK, "visible as soon as access is");
    assert_eq!(resp.bytes().await.unwrap(), manifest.as_slice());
    assert_eq!(fake.reg.tokens_issued(), 1, "the cached token was reused");
}

#[tokio::test]
async fn token_realm_on_link_local_is_refused() {
    let fake = fake_with_image(Options {
        challenge: true,
        realm: Some("http://169.254.169.254/token".into()),
        ..Default::default()
    })
    .await;
    let a = spawn_fake_proxy(&fake.reg, ProxyOpts::default(), "10s").await;

    let resp = get(&format!(
        "{}/v2/oci-proxy/{IMAGE}/manifests/1.0",
        a.base_url
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("upstream token realm refused"),
        "the guard, not another failure, answered: {body}"
    );
    assert!(cache_rows(&a).await.is_empty());
}

/// A realm on a loopback host other than the upstream's own endpoint is
/// upstream-chosen: never contacted without the private opt-in.
#[tokio::test]
async fn token_realm_on_private_host_is_refused_without_optin() {
    let realm = fake_oci::start(Options::default()).await;
    let realm_url = format!("{}/token", realm.base_url);
    let fake = fake_with_image(Options {
        challenge: true,
        realm: Some(realm_url.clone()),
        ..Default::default()
    })
    .await;
    let url_for = |a: &TestServer| format!("{}/v2/oci-proxy/{IMAGE}/manifests/1.0", a.base_url);

    let strict = spawn_fake_proxy(&fake.reg, ProxyOpts::default(), "10s").await;
    let resp = get(&url_for(&strict)).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("upstream token realm refused"));
    assert!(realm.hits().is_empty(), "the realm was never contacted");
    assert!(cache_rows(&strict).await.is_empty());

    let lenient = spawn_fake_proxy(
        &fake.reg,
        ProxyOpts {
            dl_allow_private: true,
            ..Default::default()
        },
        "10s",
    )
    .await;
    assert_eq!(get(&url_for(&lenient)).await.status(), StatusCode::OK);
    assert_eq!(realm.tokens_issued(), 1);
}

#[tokio::test]
async fn token_realm_off_upstream_host_gets_no_credentials() {
    let realm = fake_oci::start(Options {
        realm_basic: Some(("u".into(), "p".into())),
        ..Default::default()
    })
    .await;
    let realm_url = format!("{}/token", realm.base_url);
    let fake = fake_with_image(Options {
        challenge: true,
        realm: Some(realm_url.clone()),
        ..Default::default()
    })
    .await;
    let url_for = |a: &TestServer| format!("{}/v2/oci-proxy/{IMAGE}/manifests/1.0", a.base_url);

    let a = spawn_fake_proxy(
        &fake.reg,
        ProxyOpts {
            dl_allow_private: true,
            ..basic("u", "p")
        },
        "10s",
    )
    .await;
    assert_eq!(get(&url_for(&a)).await.status(), StatusCode::BAD_GATEWAY);
    let token_hits: Vec<_> = realm
        .hits()
        .into_iter()
        .filter(|h| h.path.starts_with("/token"))
        .collect();
    assert_eq!(token_hits.len(), 1);
    assert!(
        !token_hits[0].headers.contains_key("authorization"),
        "queried anonymously"
    );

    let trusted = spawn_fake_proxy(
        &fake.reg,
        ProxyOpts {
            token_realms: vec![realm_url],
            ..basic("u", "p")
        },
        "10s",
    )
    .await;
    assert_eq!(
        get(&url_for(&trusted)).await.status(),
        StatusCode::OK,
        "a listed realm sees them"
    );
    assert_eq!(realm.tokens_issued(), 1);
}
