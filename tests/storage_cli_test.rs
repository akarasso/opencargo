//! `opencargo storage check|verify|migrate|reclaim`, through the
//! composition root functions the subcommands call. Under
//! `OPENCARGO_TEST_STORAGE=s3` both stores are S3, under two prefixes.

mod common;

fn listing() -> opencargo::app::storage_ops::Verify {
    opencargo::app::storage_ops::Verify {
        orphans: true,
        ..opencargo::app::storage_ops::Verify::default()
    }
}

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::json;

use common::{
    build_npm_publish_body, build_tarball, hosted, push_blob, respawn, spawn_server,
    storage_of, stored_keys, SpawnOpts, TestServer,
};
use opencargo::config::{Config, RepositoryFormat, StorageConfig, Visibility};
use opencargo::server;

fn opts() -> SpawnOpts {
    SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            hosted("oci-hosted", RepositoryFormat::Oci, Visibility::Public),
        ],
        lease: true,
        ..Default::default()
    }
}

/// The config a storage command runs with: the server's, lease on.
fn command_config(server: &TestServer) -> Config {
    let mut config = common::config_of(server);
    config.server.lease = true;
    config
}

async fn publish_npm(client: &reqwest::Client, base_url: &str) {
    let tarball = build_tarball(r#"{"name":"left-pad","version":"1.0.0"}"#);
    let body = build_npm_publish_body("left-pad", "1.0.0", "pads", &tarball);
    let resp = client
        .put(format!("{base_url}/npm-hosted/left-pad"))
        .bearer_auth(common::STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn push_image(client: &reqwest::Client, base_url: &str) -> String {
    let layer = push_blob(client, base_url, "oci-hosted/app", b"layer bytes").await;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": layer, "size": 11},
        "layers": []
    }))
    .unwrap();
    let resp = client
        .put(format!("{base_url}/v2/oci-hosted/app/manifests/v1"))
        .bearer_auth(common::STATIC_TOKEN)
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    layer
}

async fn stop(server: &mut TestServer) {
    server.stop().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.is_ok()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A store beside the server's, for `migrate --to`: another directory, or
/// another prefix of the same bucket.
fn target_config(server: &TestServer) -> Config {
    let mut target = common::config_of(server);
    if common::storage_is_s3() {
        target.storage.s3.prefix = format!("{}-migrated", server.storage.s3.prefix);
    } else {
        target.storage = StorageConfig::default();
        target.server.storage_path = server.tmp.path().join("migrated").to_string_lossy().into_owned();
    }
    target.storage.id = Some("migrated".to_string());
    target
}

#[tokio::test]
async fn migrate_then_every_format_serves_from_the_target() {
    let mut server = spawn_server(opts()).await;
    let client = reqwest::Client::new();
    publish_npm(&client, &server.base_url).await;
    let layer = push_image(&client, &server.base_url).await;
    let source = command_config(&server);
    let target = target_config(&server);

    let mut same_identity = target_config(&server);
    same_identity.storage.id = None;
    let refused = server::storage_migrate(&source, &same_identity, false).await;
    assert!(refused.unwrap_err().to_string().contains("storage.id"), "two stores without an id share one ledger scope");

    let refused = server::storage_migrate(&source, &target, false).await;
    assert!(refused.unwrap_err().to_string().contains("writer lease"), "a running server refuses migrate");
    stop(&mut server).await;

    let overlapping = common::config_of(&server);
    assert!(server::storage_migrate(&source, &overlapping, false).await.is_err(), "a store onto itself");

    let dry = server::storage_migrate(&source, &target, true).await.unwrap();
    assert!(dry.copied >= 3, "{dry:?}");
    let first_pass = server::storage_migrate(&source, &target, true).await.unwrap();
    assert_eq!(first_pass.skipped, 0, "a dry run writes nothing");

    let first = server::storage_migrate(&source, &target, false).await.unwrap();
    assert_eq!((first.copied, first.skipped), (dry.copied, 0));
    let again = server::storage_migrate(&source, &target, false).await.unwrap();
    assert_eq!(again.copied + again.skipped, first.copied);
    assert!(again.skipped >= 3, "content-addressed keys are proven and skipped: {again:?}");

    let mut moved = server;
    if common::storage_is_s3() {
        moved.storage = target.storage.clone();
    } else {
        let storage = moved.tmp.path().join("storage");
        std::fs::remove_dir_all(&storage).unwrap();
        std::fs::rename(moved.tmp.path().join("migrated"), &storage).unwrap();
    }
    let server = respawn(moved, opts()).await;

    let resp = client
        .get(format!("{}/npm-hosted/left-pad/-/left-pad-1.0.0.tgz", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "npm serves from the migrated store");
    let resp = client
        .get(format!("{}/v2/oci-hosted/app/blobs/{layer}", server.base_url))
        .bearer_auth(common::STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"layer bytes");
    let resp = client
        .get(format!("{}/v2/oci-hosted/app/manifests/v1", server.base_url))
        .bearer_auth(common::STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "oci manifests serve from the migrated store");
}

#[tokio::test]
async fn verify_lists_missing_keys_and_orphans_and_check_passes() {
    let mut server = spawn_server(opts()).await;
    let client = reqwest::Client::new();
    publish_npm(&client, &server.base_url).await;
    stop(&mut server).await;
    let config = common::config_of(&server);

    let report = server::storage_check(&config).await.unwrap();
    assert!(report.ok(), "{report:?}");

    let clean = server::storage_verify(&config, listing(), chrono::Utc::now()).await.unwrap();
    assert!(clean.missing.is_empty() && clean.orphans.is_empty(), "{clean:?}");

    let tarball = stored_keys(&server)
        .await
        .into_iter()
        .find(|k| k.ends_with(".tgz") || k.contains(".tgz~"))
        .expect("the tarball is stored");
    let store = storage_of(&server).await;
    store.delete(&tarball).await.unwrap();
    store
        .put("stray/object", bytes::Bytes::from_static(b"x"))
        .await
        .unwrap();

    let now = server::storage_verify(&config, listing(), chrono::Utc::now()).await.unwrap();
    assert_eq!(now.missing, vec![tarball]);
    assert!(now.orphans.is_empty(), "an object inside the grace window is in flight");
    let later = chrono::Utc::now() + chrono::TimeDelta::hours(3);
    let later = server::storage_verify(&config, listing(), later).await.unwrap();
    assert_eq!(later.orphans, vec!["stray/object".to_string()]);
}

#[tokio::test]
async fn reclaim_prefix_without_repository_row() {
    let mut server = spawn_server(opts()).await;
    stop(&mut server).await;
    let config = common::config_of(&server);
    let store = storage_of(&server).await;
    for key in ["npm/gone/p/a.tgz", "npm/gone/p/b.tgz", "npm/goner/kept.tgz"] {
        store.put(key, bytes::Bytes::from_static(b"x")).await.unwrap();
    }
    let report = server::storage_reclaim(&config, Some("npm/gone"), chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(stored_keys(&server).await, vec!["npm/goner/kept.tgz".to_string()]);
}

/// The status names the adapter and the declared identity, and never where
/// the store points; only an admin reads it.
#[tokio::test]
async fn storage_status_names_the_backend_and_no_location() {
    let server = spawn_server(opts()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/v1/system/storage", server.base_url))
        .bearer_auth(common::STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    let want = if common::storage_is_s3() { "s3" } else { "fs" };
    assert_eq!(body["backend"], want);
    assert_eq!(body["identity"], "artifacts");
    assert_eq!(body["ready"], true);
    assert_eq!(body["multipart_in_flight"], 0);
    assert_eq!(body["reclaim_candidates"], 0);
    let storage_path = server.tmp.path().to_string_lossy().into_owned();
    for secret in [storage_path.as_str(), "opencargo-test", "127.0.0.1:19000", &server.storage.s3.prefix] {
        if !secret.is_empty() {
            assert!(!text.contains(secret), "{text} names {secret}");
        }
    }

    let anonymous = client
        .get(format!("{}/api/v1/system/storage", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn migrate_refuses_while_the_writer_lease_is_held() {
    let server = spawn_server(opts()).await;
    let source = command_config(&server);
    let target = target_config(&server);
    let refused = server::storage_migrate(&source, &target, false).await.unwrap_err().to_string();
    assert!(refused.contains("writer lease"), "{refused}");
    let refused = server::storage_reclaim(&source, None, chrono::Utc::now()).await.unwrap_err().to_string();
    assert!(refused.contains("writer lease"), "reclaim takes the same lease: {refused}");
    assert!(stored_keys(&server).await.iter().all(|k| !k.contains("migrated")));
}

#[tokio::test]
async fn server_waits_on_lease_held_by_migrate() {
    let mut server = spawn_server(opts()).await;
    stop(&mut server).await;
    let config = command_config(&server);
    let stores = server::open_stores(&server.tmp.path().join("opencargo.db")).await.unwrap();
    let held = server::take_writer_lease(&config, &stores).await.unwrap().unwrap();
    let started = std::time::Instant::now();
    let releaser = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        held.release().await;
    };
    let mut config = config;
    let (booted, ()) = tokio::join!(common::start(&mut config), releaser);
    assert!(booted.is_ok(), "the server takes the lease once the command gives it back");
    assert!(started.elapsed() >= Duration::from_secs(1), "and waited for it");
}
