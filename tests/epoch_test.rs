//! The restore epoch and the high-water mark (ha-profiles C-1 to C-7), over
//! whichever store this run puts the server on.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::{Client, StatusCode};

use common::{build_npm_publish_body, build_tarball, hosted, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::app::mark::{HighWaterMark, Mark, Standing};
use opencargo::app::storage_ops::Verify;
use opencargo::backup::lock::{restore_lock_guard, Lock};
use opencargo::backup::StatvfsProbe;
use opencargo::config::{Config, RepositoryFormat, Visibility};
use opencargo::domain::layout;
use opencargo::ports::reclaim::{Epoch, Pinned, ReclaimStore};
use opencargo::server::{self, BackupArgs};

fn opts() -> SpawnOpts {
    SpawnOpts {
        repositories: vec![hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public)],
        ..Default::default()
    }
}

fn db_path(server: &TestServer) -> PathBuf {
    server.tmp.path().join("opencargo.db")
}

fn args(to: &Path) -> BackupArgs {
    BackupArgs {
        to: to.to_path_buf(),
        storage: true,
        force: false,
        keep: 7,
        space: Arc::new(StatvfsProbe),
    }
}

async fn publish(base_url: &str, name: &str, version: &str) {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"{version}"}}"#));
    let body = build_npm_publish_body(name, version, "d", &tarball);
    let resp = Client::new()
        .put(format!("{base_url}/npm-hosted/{name}"))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn restore(config: &Config, from: &Path) -> server::RestoreReport {
    let db = server::database_path(config).unwrap();
    let lock = restore_lock_guard(&db, Lock::Exclusive(from)).unwrap();
    server::run_restore(config, from, true, lock).await.unwrap()
}

async fn epoch_of(server: &TestServer) -> Epoch {
    let stores = server::open_stores(&db_path(server)).await.unwrap();
    let state = stores.reclaim().epoch().await.unwrap();
    stores.close().await;
    state
}

async fn stores_of(server: &TestServer) -> Arc<dyn ReclaimStore> {
    server::open_stores(&db_path(server)).await.unwrap().reclaim()
}

/// C-1, C-2: a restore is a branch of its own, so what it references is
/// never placed at again, and the bytes it restored are still served.
#[tokio::test]
async fn a_restore_draws_a_fresh_epoch_and_places_at_a_fresh_generation() {
    let mut server = spawn_server(opts()).await;
    publish(&server.base_url, "left-pad", "1.0.0").await;
    let config = common::config_of(&server);
    let to = server.tmp.path().join("backups");
    let snapshot = server::run_backup(&config, args(&to)).await.unwrap();
    let before = epoch_of(&server).await;
    server.stop().await;

    restore(&config, &snapshot.dir).await;
    let after = epoch_of(&server).await;
    assert_ne!(after.epoch, before.epoch, "a restore draws an opaque epoch");
    assert_eq!(after.installation, before.installation);
    assert!(after.verify_pending, "and owes a verify before it reclaims");

    let stored = common::stored_keys(&server).await;
    let tarball = stored
        .iter()
        .find(|k| k.contains(".tgz"))
        .expect("the restored tarball is there")
        .clone();
    let logical = layout::logical_key(&tarball).to_string();
    let prefix = logical.rsplitn(4, '/').last().unwrap().to_string();
    let reclaim = stores_of(&server).await;
    let Pinned::Tokens(tokens) = reclaim.pin(&prefix, &[logical], chrono::Utc::now() + chrono::TimeDelta::hours(1)).await.unwrap() else {
        panic!("the incarnation is live")
    };
    assert_ne!(
        tokens[0].physical_key, tarball,
        "a generation of the epoch before the restore is never reused"
    );
    assert_eq!(
        layout::generation_epoch(&tokens[0].physical_key),
        Some(after.epoch.as_str())
    );
    let storage = common::storage_of(&server).await;
    assert!(storage.stat(&tarball).await.unwrap().is_some(), "the restored bytes are there");
}

/// I7: what was placed after the backup point has no row after the restore,
/// so the verify the epoch owes queues it — and never deletes it itself.
#[tokio::test]
async fn bytes_placed_after_the_backup_point_are_queued_by_the_verify() {
    let mut server = spawn_server(opts()).await;
    publish(&server.base_url, "kept", "1.0.0").await;
    let config = common::config_of(&server);
    let to = server.tmp.path().join("backups");
    let snapshot = server::run_backup(&config, args(&to)).await.unwrap();
    publish(&server.base_url, "later", "1.0.0").await;
    let after_backup = common::stored_keys(&server)
        .await
        .into_iter()
        .find(|k| k.contains("later"))
        .expect("the second publish is stored");
    server.stop().await;

    restore(&config, &snapshot.dir).await;
    let later = chrono::Utc::now() + chrono::TimeDelta::hours(3);
    let report = server::storage_verify(&config, Verify { orphans: true, ..Verify::default() }, later)
        .await
        .unwrap();
    assert!(report.missing.is_empty(), "{report:?}");
    assert!(
        report.enqueued.contains(&after_backup),
        "the verify queues what the restore orphaned: {report:?}"
    );
    let reclaim = stores_of(&server).await;
    assert!(reclaim.backlog().await.unwrap().candidates >= 1);
    assert!(
        !reclaim.epoch().await.unwrap().verify_pending,
        "the verify lifts the refusal it was owed"
    );
    let storage = common::storage_of(&server).await;
    assert!(
        storage.stat(&after_backup).await.unwrap().is_some(),
        "a verify reports and queues, it never deletes"
    );
}

/// C-3, C-6: the mark is compared both ways, and a crash between the mark
/// and the counter it advances leaves the conservative case.
#[tokio::test]
async fn the_mark_is_compared_both_ways_and_a_crash_leaves_the_safe_one() {
    let mut server = spawn_server(opts()).await;
    publish(&server.base_url, "marked", "1.0.0").await;
    server.stop().await;
    let storage = common::storage_of(&server).await;
    let reclaim = stores_of(&server).await;
    let mark = HighWaterMark::new(reclaim.clone(), storage.clone());

    assert!(mark.permit().await.unwrap(), "a level installation may reclaim");
    let state = reclaim.epoch().await.unwrap();
    assert_eq!(state.counter, 1, "the counter follows the mark");
    assert_eq!(mark.read().await.unwrap().unwrap().counter, 1);

    // The mark is written before the counter: this is what a crash between
    // the two leaves behind.
    storage
        .put(
            layout::MARK,
            serde_json::to_vec(&Mark {
                installation: state.installation.clone(),
                epoch: state.epoch.clone(),
                counter: state.counter + 1,
            })
            .unwrap()
            .into(),
        )
        .await
        .unwrap();
    assert_eq!(mark.guard().await.unwrap(), Standing::DatabaseBehind);
    let drawn = reclaim.epoch().await.unwrap();
    assert_ne!(drawn.epoch, state.epoch, "the conservative case: a fresh epoch");
    assert!(drawn.verify_pending);
    assert!(!mark.permit().await.unwrap(), "and no reclamation until the verify");

    let config = common::config_of(&server);
    server::storage_verify(&config, Verify::default(), chrono::Utc::now()).await.unwrap();
    assert!(mark.permit().await.unwrap(), "the verify lifts the refusal");

    storage.delete(layout::MARK).await.unwrap();
    assert_eq!(mark.guard().await.unwrap(), Standing::StorageBehind);
    let standing = reclaim.epoch().await.unwrap();
    assert_eq!(standing.epoch, drawn.epoch, "a store behind draws no epoch");
    assert!(standing.verify_pending);
}

/// C-4, C-13: the store's own tree is nobody's candidate.
#[tokio::test]
async fn the_reserved_tree_is_never_scanned_or_reclaimed() {
    let mut server = spawn_server(opts()).await;
    publish(&server.base_url, "scanned", "1.0.0").await;
    server.stop().await;
    let config = common::config_of(&server);
    let storage = common::storage_of(&server).await;
    let reclaim = stores_of(&server).await;
    HighWaterMark::new(reclaim, storage.clone()).permit().await.unwrap();
    assert!(storage.stat(layout::MARK).await.unwrap().is_some());

    let later = chrono::Utc::now() + chrono::TimeDelta::hours(3);
    let report = server::storage_reclaim(&config, None, later).await.unwrap();
    assert!(!report.refused, "{report:?}");
    assert_eq!(report.scan_orphans, 0, "the mark is no orphan: {report:?}");
    assert!(storage.stat(layout::MARK).await.unwrap().is_some(), "and is never deleted");

    let verify = server::storage_verify(&config, Verify { orphans: true, ..Verify::default() }, later)
        .await
        .unwrap();
    assert!(
        !verify.orphans.iter().any(|k| layout::reserved(k)),
        "nor listed by a verify: {verify:?}"
    );
}
